use std::{cell::RefCell, collections::{HashMap, HashSet}, fs::File, io::{self, BufReader}, path::Path, rc::Rc};
use std::fmt::{Display, Formatter};
use itertools::Itertools;
use serde::Serialize;
use vcd::{Command as Command, IdCode, Value, Vector};
use anyhow::Result;
use log::{debug, info};
use crate::{pdg_spec::{PDGSpec, PDGSpecEdge, PDGSpecEdgeKind, PDGSpecNode, PDGSpecNodeKind}, errors::Error};

/// Main structure for building dynamic program dependence graphs (DPDGs) from VCD traces.
///
/// This processes a static PDG specification along with VCD simulation data to produce
/// a dynamic graph showing actual dependencies during execution.
pub struct GraphBuilder {
    reader: VcdReader,
    pdg: PDGSpec,
    /// PDG nodes with bidirectional adjacency lists for efficient traversal
    linked_nodes: Vec<Rc<RefCell<PDGNode>>>,
    /// Current values of predicate variables (control flow conditions)
    pred_values: HashMap<IdCode, SimulationValue>,
    /// Maps predicate indices to their VCD ID codes
    pred_idx_to_id: Vec<IdCode>,
    /// Tracks the most recent node that assigned to each signal/variable
    dependency_state: HashMap<String, Rc<RefCell<DynPDGNode>>>,
}

/// Reads and parses VCD files, tracking signal changes and clock edges.
struct VcdReader {
    parser: vcd::Parser<io::BufReader<File>>,
    /// Hierarchical scope path to the design under test
    extra_scopes: Vec<String>,
    header: vcd::Header,
    clock: vcd::IdCode,
    reset: vcd::IdCode,
    reset_val: vcd::Value,
    current_time: i64,
    clock_val: vcd::Value,
    /// Buffers changes that occur at the same timestamp as a clock edge
    changes_buffer: Vec<ValueChange>,
    /// Maps VCD IDs to probe signal paths
    probes: HashMap<IdCode, Vec<String>>,
    /// Current values of all probe signals
    probe_values: HashMap<String, u64>,
    /// Buffers probe changes for processing
    probe_change_buffer: Vec<(String, u64)>,
}

/// Represents a signal value change in the VCD trace.
#[derive(Debug, Clone)]
struct ValueChange {
    id: vcd::IdCode,
    value: SimulationValue,
}

#[derive(Debug, Clone)]
enum SimulationValue {
    Scalar(vcd::Value),
    Vector(vcd::Vector),
    Real(f64),
    String(String),
}

impl Display for SimulationValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SimulationValue::Scalar(s) => {s.fmt(f)}
            SimulationValue::Vector(v) => {v.fmt(f)}
            SimulationValue::Real(r) => {r.fmt(f)}
            SimulationValue::String(s) => {s.fmt(f)}
        }
    }
}

/// Static PDG node enriched with adjacency lists for efficient graph traversal.
#[derive(Debug)]
struct PDGNode {
    inner: Rc<PDGSpecNode>,
    /// Nodes that this node provides data/control to
    provides: Vec<(Rc<RefCell<PDGNode>>, PDGSpecEdge)>,
    /// Nodes that this node depends on
    dependencies: Vec<(Rc<RefCell<PDGNode>>, PDGSpecEdge)>,
}

/// A node in the dynamic PDG (DPDG) representing an executed statement.
///
/// Note: If cycles exist in the graph, the Rc pointers will leak memory.
/// This shouldn't happen in a valid dependence graph.
#[derive(Debug, Serialize)]
pub struct DynPDGNode {
    pub inner: Rc<PDGSpecNode>,
    /// Simulation time when this statement executed
    pub timestamp: i64,
    /// Dynamic dependencies to other executed statements
    pub dependencies: Vec<(Rc<RefCell<DynPDGNode>>, PDGSpecEdgeKind)>,
}

/// Specifies what to trace in the simulation.
#[derive(Debug, Clone)]
pub enum CriterionType {
    /// Trace back from a specific statement by name
    Statement(String),
    /// Trace back from a specific signal by name
    Signal(String),
}

/// Used to configure behaviour and expected paths based on the source language of the circuit.
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum LanguageMode {
    Chisel,
    FIR,
    SpinalHDL
}

/// Controls which types of dependencies to include in the dynamic graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphProcessingType {
    /// Standard SigTrail: data, control flow, and index dependencies
    Normal,
    /// Data dependencies only for smaller graphs
    DataOnly,
    /// Include statement definitions for full dynamic program slicing
    Full,
}

impl GraphBuilder {
    /// Creates a new GraphBuilder from a VCD file and PDG specification.
    ///
    /// This preprocesses the static PDG by creating bidirectional adjacency lists
    /// using hash maps for O(1) edge lookup instead of O(n) iteration.
    pub fn new(vcd_path: impl AsRef<Path>, extra_scopes: Vec<String>, pdg: PDGSpec, language_mode: LanguageMode) -> Result<GraphBuilder> {
        let vcd_reader = VcdReader::new(vcd_path, extra_scopes, language_mode)?;

        // Wrap PDG nodes in refcounted cells for shared mutable access during traversal
        let linked = pdg.vertices.iter().map(|v| {
            Rc::new(RefCell::new(PDGNode { inner: Rc::new(v.clone()), provides: vec![], dependencies: vec![] }))
        }).collect::<Vec<_>>();

        // Build adjacency maps indexed by node ID for efficient edge lookup
        let mut edges_by_from: HashMap<u32, Vec<_>> = HashMap::new();
        let mut edges_by_to: HashMap<u32, Vec<_>> = HashMap::new();
        for edge in &pdg.edges {
            edges_by_from.entry(edge.from).or_default().push(edge);
            edges_by_to.entry(edge.to).or_default().push(edge);
        }

        // Populate adjacency lists in both directions
        for (node_idx, node) in linked.iter().enumerate() {
            for edge in edges_by_from.get(&(node_idx as u32)).into_iter().flatten() {
                let mut node_ref = node.borrow_mut();
                node_ref.dependencies.push((linked[edge.to as usize].clone(), (*edge).clone()));
            }
            for edge in edges_by_to.get(&(node_idx as u32)).into_iter().flatten() {
                node.borrow_mut().provides.push((linked[edge.from as usize].clone(), (*edge).clone()));
            }
        }

        Ok(GraphBuilder { reader: vcd_reader, pdg, linked_nodes: linked, pred_values: HashMap::new(), pred_idx_to_id: vec![], dependency_state: HashMap::new() })
    }

    /// Processes the VCD trace to build a dynamic PDG up to the specified criterion.
    ///
    /// This is the main algorithm that:
    /// 1. Reads VCD changes cycle by cycle
    /// 2. Determines which statements are executed based on control flow
    /// 3. Resolves dynamic dependencies between executed statements
    /// 4. Handles clocked vs. combinational logic timing
    /// 5. Returns the DPDG node matching the criterion
    pub fn process(&mut self, criterion: &CriterionType, max_timesteps: Option<i64>, processing_type: GraphProcessingType) -> Result<Rc<RefCell<DynPDGNode>>> {
        self.init_predicates()?;

        let mut eof_reached = false;
        let mut criterion_node = None;

        // Tracks statements with delayed assignments (e.g., sequential memory)
        let mut delayed_statement_buffer: Vec<(i64, u32)> = vec![];

        // Snapshots of dependency state for resolving delayed assignments
        let mut dependency_state_snapshots: HashMap<i64, (HashMap<String, Rc<RefCell<DynPDGNode>>>, HashMap<String, u64>)> = HashMap::new();

        // Process simulation cycle by cycle
        while !eof_reached && self.reader.current_time * 2 <= max_timesteps.unwrap_or(i64::MAX) {
            let (c, eof) = self.reader.read_cycle_changes()?;
            let corrected_timestamp = self.reader.current_time - 1;
            eof_reached = eof;
            let activated_statements = self.get_activated_statements(&c);

            // Track which nodes provide values to registers and control flow this cycle
            let mut new_reg_providers: HashMap<String, Rc<RefCell<DynPDGNode>>> = HashMap::new();
            let mut controlflow_providers: HashMap<Rc<PDGSpecNode>, Rc<RefCell<DynPDGNode>>> = HashMap::new();
            let mut new_nodes = vec![];

            // Extract statements whose delayed assignments are now ready
            let mut ready_statements = vec![];
            delayed_statement_buffer = delayed_statement_buffer.into_iter().filter(|(t, stmt)| {
                if *t == corrected_timestamp {
                    ready_statements.push(*stmt);
                    false
                } else { true }
            }).collect::<Vec<_>>();

            // Separate statements into immediate and delayed based on assign_delay
            let (mut activated_statements, delayed_statements): (Vec<_>, Vec<_>) = activated_statements.into_iter().partition(|stmt| {
                let node = self.linked_nodes[*stmt as usize].borrow();
                node.inner.assign_delay == 0
            });

            let mut delayed_statements_present = false;
            for del_stmt in delayed_statements {
                let node = self.linked_nodes[del_stmt as usize].borrow();
                delayed_statement_buffer.push((corrected_timestamp + node.inner.assign_delay as i64, del_stmt));
                delayed_statements_present = true;
            }

            activated_statements.append(&mut ready_statements);

            // Create DPDG nodes for all activated statements
            for stmt in &activated_statements {
                let node = self.linked_nodes[*stmt as usize].borrow();

                // Clocked statements execute at cycle boundary, combinational logic within cycle
                let node_timestamp = if node.inner.clocked { corrected_timestamp } else { corrected_timestamp.saturating_sub(1) };
                let dpdg_node = Rc::new(RefCell::new(DynPDGNode { inner: node.inner.clone(), timestamp: node_timestamp, dependencies: vec![] }));
                new_nodes.push((self.linked_nodes[*stmt as usize].clone(), dpdg_node.clone()));

                // Check if the statement's execution condition is satisfied by probe values
                let conditions_satisfied = if let Some(conds) = &node.inner.condition {
                    conds.probe_name.iter().zip(&conds.probe_value).all(|(probe, required_value)| {
                        if let Some(current_probe_val) = self.reader.probe_values.get(probe) {
                            *required_value == *current_probe_val
                        } else {
                            false
                        }
                    })
                } else {
                    true
                };

                // Update dependency state to track which statement most recently assigned to each signal
                if conditions_satisfied {
                    if let Some(symb) = &node.inner.assigns_to {
                        if node.inner.clocked {
                            // Handle register initialization/reset specially
                            if node.inner.kind == PDGSpecNodeKind::DataDefinition {
                                if corrected_timestamp == 0 || self.reader.reset_val == vcd::Value::V1 {
                                    dpdg_node.borrow_mut().timestamp -= 1;
                                    self.dependency_state.insert(symb.clone(), dpdg_node.clone());
                                }
                            } else {
                                // Register updates are buffered and applied at cycle end
                                new_reg_providers.insert(symb.clone(), dpdg_node.clone());
                            }
                        } else {
                            // Combinational logic updates immediately
                            self.dependency_state.insert(symb.clone(), dpdg_node.clone());
                        }
                    }

                    if node.inner.kind == PDGSpecNodeKind::ControlFlow {
                        controlflow_providers.insert(node.inner.clone(), dpdg_node.clone());
                    }
                }
            }

            // Resolve dependencies for each newly created DPDG node
            for (node, dpdg_node) in &new_nodes {
                let node_delay = node.borrow().inner.assign_delay;

                // For delayed assignments, use snapshotted state from when the assignment was initiated
                let (dep_state, probe_vals) = if node_delay > 0 {
                    let x = &dependency_state_snapshots[&(corrected_timestamp - node_delay as i64)];
                    (&x.0, &x.1)
                } else {
                    (&self.dependency_state, &self.reader.probe_values)
                };

                // Track which symbols we've already processed to avoid duplicate dependencies
                let mut deps_processed = HashSet::new();

                // Iterate through static dependencies and resolve them to dynamic nodes
                for (dep_node, dep_edge) in &node.borrow().dependencies {
                    if let Some(ref assigns_to) = dep_node.borrow().inner.assigns_to {
                        if deps_processed.contains(assigns_to) {
                            continue;
                        }
                    }

                    if processing_type == GraphProcessingType::DataOnly && dep_edge.kind != PDGSpecEdgeKind::Data {
                        continue;
                    }

                    // Check if the edge's condition is satisfied
                    let conditions_satisfied = if let Some(conds) = &dep_edge.condition {
                        conds.probe_name.iter().zip(&conds.probe_value).all(|(probe, required_value)| {
                            if let Some(current_probe_val) = probe_vals.get(probe) {
                                *required_value == *current_probe_val
                            } else {
                                false
                            }
                        })
                    } else {
                        true
                    };

                    if conditions_satisfied {
                        match dep_edge.kind {
                            PDGSpecEdgeKind::Declaration => {
                                // Declaration edges only needed for full program slicing
                                if processing_type == GraphProcessingType::Full {
                                    let dep = Rc::new(RefCell::new(DynPDGNode { inner: dep_node.borrow().inner.clone(), timestamp: corrected_timestamp - 1, dependencies: vec![] }));
                                    dpdg_node.borrow_mut().dependencies.push((dep.clone(), dep_edge.kind));
                                }
                            }
                            PDGSpecEdgeKind::Data | PDGSpecEdgeKind::Index => {
                                // Data dependencies always use current state, index deps may use snapshots
                                let dep_state = if dep_edge.kind == PDGSpecEdgeKind::Data {
                                    &self.dependency_state
                                } else {
                                    dep_state
                                };
                                if let Some(dep_str) = &dep_node.borrow().inner.assigns_to {
                                    if let Some(dep) = dep_state.get(dep_str) {
                                        dpdg_node.borrow_mut().dependencies.push((dep.clone(), dep_edge.kind));
                                    }
                                    deps_processed.insert(dep_str.clone());
                                }
                            }
                            PDGSpecEdgeKind::Conditional => {
                                if let Some(cond_dep) = controlflow_providers.get(&dep_node.borrow().inner) {
                                    dpdg_node.borrow_mut().dependencies.push((cond_dep.clone(), PDGSpecEdgeKind::Conditional));
                                }
                            }
                            _ => ()
                        }
                    }
                }
            }

            // Save state snapshot for resolving future delayed assignments
            if delayed_statements_present {
                dependency_state_snapshots.insert(corrected_timestamp, (self.dependency_state.clone(), self.reader.probe_values.clone()));
            }

            // Check if any new nodes match the tracing criterion
            for (_, n) in new_nodes {
                if match criterion {
                    CriterionType::Statement(c) => n.borrow().inner.name.eq(c),
                    CriterionType::Signal(c) => n.borrow().inner.assigns_to.as_ref().map_or(false, |s| s.eq(c))
                } {
                    criterion_node = Some(n)
                }
            }

            // Apply buffered register updates
            for (k, v) in new_reg_providers {
                self.dependency_state.insert(k, v);
            }
        }

        // Return the node matching the criterion
        let exported_node = match criterion {
            CriterionType::Statement(_) => criterion_node.as_ref(),
            CriterionType::Signal(c) => self.dependency_state.get(c)
        }.ok_or(Error::StatementLookupError("Criterion not found in DPDG".into()))?;

        Ok(exported_node.clone())
    }

    /// Initializes predicate tracking by mapping PDG predicates to VCD signal IDs.
    fn init_predicates(&mut self) -> Result<()> {
        for pred in &self.pdg.predicates {
            let pred_id = self.reader.find_var(&pred.name)?;
            self.pred_values.insert(pred_id, SimulationValue::Scalar(vcd::Value::X));
            self.pred_idx_to_id.push(pred_id);
            debug!("Initialized predicate: {} ↔ {} @ {}:{}", pred_id, pred.name, pred.file, pred.line);
        }

        Ok(())
    }

    /// Determines which statements executed this cycle by traversing the control flow graph.
    ///
    /// Uses a stack-based traversal, following branches based on predicate values.
    fn get_activated_statements(&mut self, changes: &Vec<ValueChange>) -> Vec<u32> {
        // Update predicate values based on signal changes
        for change in changes {
            if let Some(v) = self.pred_values.get_mut(&change.id) {
                *v = change.value.clone();
            }
        }

        let mut activated = Vec::new();

        // Traverse CFG depth-first using a stack
        let mut stack = self.pdg.cfg.clone();
        stack.reverse();

        while let Some(node) = stack.pop() {
            activated.push(node.stmt_ref);

            // Follow conditional branches based on predicate values
            if let Some(pred) = node.pred_stmt_ref {
                let pred_id = self.pred_idx_to_id[pred as usize];
                let pred_value = self.pred_values[&pred_id].clone();
                let pred_active = match pred_value {
                    SimulationValue::Scalar(v) => {
                        let active = v == vcd::Value::V1;
                        debug!("Predicate {} {} is {} @ {}:{}", pred_id, self.pdg.predicates[pred as usize].name, if active { "active" } else { "inactive" }, self.pdg.predicates[pred as usize].file, self.pdg.predicates[pred as usize].line);
                        active
                    }
                    _ => false
                };
                if pred_active {
                    if let Some(t_branch) = node.true_branch {
                        stack.extend(t_branch.into_iter().rev());
                    }
                } else if let Some(f_branch) = node.false_branch {
                    stack.extend(f_branch.into_iter().rev());
                }
                if let Some(branches) = node.branches {
                    let pred_value_string = pred_value.to_string();
                    debug!("Predicate {} {} is {} @ {}:{}", pred_id, self.pdg.predicates[pred as usize].name, pred_value_string, self.pdg.predicates[pred as usize].file, self.pdg.predicates[pred as usize].line);
                    let active_branches = branches.into_iter().filter(|b| b.match_values.iter().any(|v| *v == pred_value_string));
                    let mut has_active_branch = false;
                    for branch in active_branches {
                        stack.extend(branch.stmts.into_iter().rev());
                        has_active_branch = true;
                    }
                    if !has_active_branch && let Some(default_branch) = node.default_branch {
                        stack.extend(default_branch.into_iter().rev());
                    }
                }
            }
        }

        activated
    }
}

impl VcdReader {
    /// Creates a new VCD reader and parses the header to locate clock and reset signals.
    fn new(vcd_path: impl AsRef<Path>, extra_scopes: Vec<String>, language_mode: LanguageMode) -> Result<Self> {
        let file = File::open(vcd_path)?;
        let reader = BufReader::new(file);
        let mut parser = vcd::Parser::new(reader);
        let header = parser.parse_header()?;

        let mut clock_path = extra_scopes.clone();
        let clock_name = if let LanguageMode::SpinalHDL = language_mode { "clk".into() } else { "clock".into() };
        clock_path.push(clock_name);

        let mut reset_path = extra_scopes.clone();
        reset_path.push("reset".into());

        let clock = header.find_var(&clock_path).ok_or(Error::ClockNotFoundError)?.code;
        let reset = header.find_var(&reset_path).ok_or(Error::ClockNotFoundError)?.code;

        let probes = Self::find_probes(&header, &extra_scopes);

        Ok(VcdReader { parser, extra_scopes, header, clock, reset, reset_val: vcd::Value::X, current_time: 0, clock_val: vcd::Value::X, changes_buffer: vec![], probes, probe_values: HashMap::new(), probe_change_buffer: vec![] })
    }

    /// Recursively searches the VCD hierarchy for probe signals (signals starting with "probe_").
    ///
    /// Returns a map from VCD ID to probe paths, handling cases where multiple probes
    /// share the same ID.
    fn find_probes(header: &vcd::Header, root_scope: &[String]) -> HashMap<IdCode, Vec<String>> {
        let mut probes = HashMap::new();
        if let Some(dut) = header.find_scope(root_scope) {
            // Stack-based traversal of VCD hierarchy: (path_prefix, scope_item)
            let mut stack = vec![];
            stack.extend_from_slice(&dut.items.iter().map(|i| ("".to_string(), i)).collect::<Vec<_>>());

            while let Some((prefix, item)) = stack.pop() {
                match item {
                    vcd::ScopeItem::Scope(scope) => {
                        let new_prefix = if prefix.is_empty() {
                            scope.identifier.clone()
                        } else {
                            prefix.to_string() + "." + &scope.identifier
                        };
                        stack.extend_from_slice(&scope.items.iter().map(|i| (new_prefix.clone(), i)).collect::<Vec<_>>());
                    }
                    vcd::ScopeItem::Var(var) => {
                        if var.reference.starts_with("probe_") {
                            let probe_path = if prefix.is_empty() {
                                var.reference.clone()
                            } else {
                                prefix.clone() + "." + &var.reference
                            };
                            probes.entry(var.code).and_modify(|e: &mut Vec<String>| e.push(probe_path.clone())).or_insert(vec![probe_path]);
                        }
                    }
                    _ => ()
                }
            }
        }

        probes
    }

    /// Finds a variable in the VCD hierarchy given a dot-separated path.
    fn find_var(&self, hierarchy: impl AsRef<str>) -> Result<IdCode> {
        let mut hier_path = self.extra_scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>();
        hier_path.extend(hierarchy.as_ref().split("."));
        Ok(self.header.find_var(&hier_path).ok_or(Error::VariableNotFoundError(hier_path.join(".")))?.code)
    }

    /// Reads VCD events until the next clock rising edge.
    ///
    /// Returns all signal changes that occurred during this clock cycle and whether EOF was reached.
    /// Buffers changes that occur at the same timestamp as the clock edge to process them
    /// in the next cycle (modeling combinational delay).
    fn read_cycle_changes(&mut self) -> Result<(Vec<ValueChange>, bool)> {
        let mut changes = vec![];
        let mut rising_edge_found = false;
        let mut eof_reached = true;
        let last_time = self.current_time;

        for command in self.parser.by_ref() {
            let command = command?;
            match command {
                Command::Timestamp(_t) => {
                    // Changes at same timestamp as rising edge are processed next cycle
                    if rising_edge_found {
                        self.current_time += 1;
                        info!("Processing cycle {} at time {}", self.current_time, _t);
                        eof_reached = false;
                        break;
                    } else {
                        changes.append(&mut self.changes_buffer);
                        for change in &self.probe_change_buffer {
                            self.probe_values.insert(change.0.clone(), change.1);
                        }
                        self.probe_change_buffer.clear();
                    }
                }
                Command::ChangeScalar(i, v) if i == self.clock => {
                    if self.clock_val == vcd::Value::V0 && v == vcd::Value::V1 {
                        rising_edge_found = true;
                    }
                    self.clock_val = v;
                    debug!("Clock: {}", if let vcd::Value::V1 = v { "high" } else { "low" });
                }
                Command::ChangeScalar(i, v) if i == self.reset => {
                    self.reset_val = v;
                    debug!("Reset: {}", if let vcd::Value::V1 = v { "high" } else { "low" });
                }
                Command::ChangeScalar(i, v) => {
                    if let Some(probes) = self.probes.get(&i) {
                        for probe in probes {
                            let unsigned_v = match v {
                                vcd::Value::V1 => 1,
                                _ => 0
                            };
                            self.probe_change_buffer.push((probe.clone(), unsigned_v));
                            info!("Probe change: {} = {}", probe, unsigned_v);
                        }
                    } else {
                        self.changes_buffer.push(ValueChange { id: i, value: SimulationValue::Scalar(v) });
                    }
                }
                Command::ChangeVector(i, v) => {
                    if let Some(probes) = self.probes.get(&i) {
                        for probe in probes {
                            self.probe_change_buffer.push((probe.clone(), bitvector_to_unsigned(&v)));
                        }
                    } else {
                        self.changes_buffer.push(ValueChange { id: i, value: SimulationValue::Vector(v) });
                    }
                }
                _ => ()
            }
        }

        // Handle case where no rising edge was found (end of simulation)
        if last_time == self.current_time {
            self.current_time += 1;
        }

        Ok((changes, eof_reached))
    }
}

/// Converts a VCD bit vector to an unsigned integer value.
fn bitvector_to_unsigned(input_vec: &vcd::Vector) -> u64 {
    let mut val = 0;
    let mut bitval = 1;

    let mut rev_bits = input_vec.iter().collect::<Vec<_>>();
    rev_bits.reverse();

    for input in rev_bits {
        if input == vcd::Value::V1 {
            val += bitval;
        }
        bitval <<= 1;
    }
    val
}