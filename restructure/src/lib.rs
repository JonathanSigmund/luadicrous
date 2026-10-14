use cfg::{block::BranchType, function::Function};
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};

use petgraph::{
    algo::dominators::{Dominators, simple_fast},
    stable_graph::{EdgeIndex, NodeIndex, StableDiGraph},
    visit::*,
};
use tuple::Map;

mod conditional;
mod jump;
mod r#loop;

// TODO: REFACTOR: move
pub fn post_dominators<N: Default, E: Default>(
    graph: &mut StableDiGraph<N, E>,
) -> Dominators<NodeIndex> {
    let exits = graph
        .node_identifiers()
        .filter(|&n| graph.neighbors(n).count() == 0)
        .collect_vec();
    let fake_exit = graph.add_node(Default::default());
    for exit in exits {
        graph.add_edge(exit, fake_exit, Default::default());
    }
    let res = simple_fast(Reversed(&*graph), fake_exit);
    assert!(graph.remove_node(fake_exit).is_some());
    res
}

struct GraphStructurer {
    pub function: Function,
    loop_headers: FxHashSet<NodeIndex>,
    label_to_node: FxHashMap<ast::Label, NodeIndex>,
    duplicated_statements: usize,
}

impl GraphStructurer {
    fn find_loop_headers(&mut self) {
        self.loop_headers.clear();
        depth_first_search(
            self.function.graph(),
            Some(self.function.entry().unwrap()),
            |event| {
                if let DfsEvent::BackEdge(_, header) = event {
                    self.loop_headers.insert(header);
                }
            },
        );
    }
    fn new(function: Function) -> Self {
        let mut this = Self {
            function,
            loop_headers: FxHashSet::default(),
            label_to_node: FxHashMap::default(),
            duplicated_statements: 0,
        };
        this.find_loop_headers();
        this
    }

    fn block_is_no_op(block: &ast::Block) -> bool {
        !block.iter().any(|s| s.as_comment().is_none())
    }

    fn try_match_pattern(
        &mut self,
        node: NodeIndex,
        dominators: &Dominators<NodeIndex>,
        post_dom: &Dominators<NodeIndex>,
    ) -> bool {
        let successors = self.function.successor_blocks(node).collect_vec();

        // cfg::dot::render_to(&self.function, &mut std::io::stdout()).unwrap();
        if self.try_collapse_terminal_conditional(node) {
            self.find_loop_headers();
            return true;
        }

        if self.try_collapse_loop(node, dominators, post_dom) {
            self.find_loop_headers();
            // println!("matched loop");
            return true;
        }

        if self.try_remove_unnecessary_condition(node) {
            return true;
        }

        let changed = match successors.len() {
            0 => false,
            1 => {
                // remove unnecessary jumps to allow pattern matching
                self.match_jump(node, Some(successors[0]))
            }
            2 => {
                let (then_target, else_target) = self
                    .function
                    .conditional_edges(node)
                    .unwrap()
                    .map(|e| e.target());
                self.match_conditional(node, then_target, else_target)
            }

            _ => unreachable!(),
        };

        //println!("after");
        //dot::render_to(&self.function, &mut std::io::stdout()).unwrap();

        changed
    }

    fn match_blocks(&mut self) -> bool {
        let dfs = Dfs::new(self.function.graph(), self.function.entry().unwrap())
            .iter(self.function.graph())
            .collect::<FxHashSet<_>>();
        let mut dfs_postorder =
            DfsPostOrder::new(self.function.graph(), self.function.entry().unwrap());
        let mut dominators = simple_fast(self.function.graph(), self.function.entry().unwrap());
        let mut post_dom = post_dominators(self.function.graph_mut());

        // cfg::dot::render_to(&self.function, &mut std::io::stdout()).unwrap();

        let mut changed = false;
        while let Some(node) = dfs_postorder.next(self.function.graph()) {
            // println!("matching {:?}", node);
            let matched = self.try_match_pattern(node, &dominators, &post_dom);
            if matched {
                dominators = simple_fast(self.function.graph(), self.function.entry().unwrap());
                post_dom = post_dominators(self.function.graph_mut());
            }
            changed |= matched;
            // if matched {
            //     cfg::dot::render_to(&self.function, &mut std::io::stdout()).unwrap();
            // }
        }

        for node in self
            .function
            .graph()
            .node_indices()
            .filter(|node| !dfs.contains(node))
            .collect_vec()
        {
            // block may have been removed in a previous iteration
            if self.function.has_block(node)
                && self.function.predecessor_blocks(node).next().is_none()
            {
                if self
                    .function
                    .block(node)
                    .unwrap()
                    .first()
                    .and_then(|s| s.as_label())
                    .is_none()
                {
                    self.function.remove_block(node);
                } else {
                    //let dominators = simple_fast(self.function.graph(), node);
                    let matched = self.try_match_pattern(node, &dominators, &post_dom);
                    changed |= matched;
                }
            }
        }

        changed
    }

    fn insert_goto_for_edge(&mut self, edge: EdgeIndex) {
        let (source, target) = self.function.graph().edge_endpoints(edge).unwrap();
        if self.function.graph().edge_weight(edge).unwrap().branch_type == BranchType::Unconditional
            && self.function.predecessor_blocks(target).count() == 1
        {
            assert!(self.function.successor_blocks(source).count() == 1);
            // TODO: this code is repeated in match_jump, move to a new function
            let edges = self.function.remove_edges(target);
            let block = self.function.remove_block(target).unwrap();
            self.function.block_mut(source).unwrap().extend(block.0);
            self.function.set_edges(source, edges);
        } else {
            // TODO: make label an Rc and have a global counter for block name
            let label = ast::Label(format!("l{}", target.index()));
            let target_block = self.function.block_mut(target).unwrap();
            if target_block.first().and_then(|s| s.as_label()).is_none() {
                self.label_to_node.insert(label.clone(), target);
                target_block.insert(0, label.clone().into());
            }
            let goto_block = self.function.new_block();
            self.function
                .block_mut(goto_block)
                .unwrap()
                .push(ast::Goto::new(label).into());

            let edge = self.function.graph_mut().remove_edge(edge).unwrap();
            self.function.graph_mut().add_edge(source, goto_block, edge);
        }
    }

    fn remove_last_return(block: ast::Block) -> ast::Block {
        if let Some(ast::Statement::Return(last_statement)) = block.last() {
            if last_statement.values.is_empty() {
                let take = block.len() - 1;
                return block.0.into_iter().take(take).collect_vec().into();
            }
        }
        block
    }

    fn collapse(&mut self) {
        loop {
            while self.match_blocks() {}
            if self.function.graph().node_count() == 1 {
                break;
            }
            // last resort refinement
            let edges = self.function.graph().edge_indices().collect::<Vec<_>>();
            // https://edmcman.github.io/papers/usenix13.pdf
            // we prefer to remove edges whose source does not dominate its target, nor whose target dominates its source
            // TODO: try all possible paths and return the one with the least gotos, i don't think there's any other way
            // to get best output
            let mut changed = false;
            for &edge in &edges {
                // edge might have been invalidated by a previous iteration due to insert_goto_for_edge
                // calling remove_block(target)
                if self.function.graph().edge_weight(edge).is_none() {
                    continue;
                }

                let (source, target) = self.function.graph().edge_endpoints(edge).unwrap();
                let dominators = simple_fast(self.function.graph(), self.function.entry().unwrap());
                let target_dominators = dominators.dominators(target);
                let source_dominators = dominators.dominators(source);
                // TODO: check if blocks in dfs instead
                if target_dominators.is_none() || source_dominators.is_none() {
                    continue;
                }
                let mut target_dominators = target_dominators.unwrap();
                let mut source_dominators = source_dominators.unwrap();
                if target_dominators.contains(&source) || source_dominators.contains(&target) {
                    continue;
                }

                self.insert_goto_for_edge(edge);
                self.find_loop_headers();
                changed = self.match_blocks();
                if changed {
                    break;
                }
            }

            if !changed {
                for edge in edges {
                    // edge might have been invalidated by a previous iteration due to insert_goto_for_edge
                    // calling remove_block(target)
                    if self.function.graph().edge_weight(edge).is_none() {
                        continue;
                    }
                    self.insert_goto_for_edge(edge);
                    self.find_loop_headers();
                    changed = self.match_blocks();
                    if changed {
                        break;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
    }

    fn structure(mut self) -> ast::Block {
        self.collapse();
        if self.function.graph().node_count() != 1 {
            let mut res_block = ast::Block::default();
            let entry = self.function.entry().unwrap();
            let mut stack = vec![entry];
            let mut visited = FxHashSet::default();
            while let Some(node) = stack.pop() {
                if visited.contains(&node) {
                    continue;
                }
                visited.insert(node);

                fn collect_gotos(block: &ast::Block, gotos: &mut FxHashSet<ast::Label>) {
                    for statement in &block.0 {
                        match statement {
                            ast::Statement::Goto(goto) => {
                                gotos.insert(goto.0.clone());
                            }
                            ast::Statement::If(r#if) => {
                                collect_gotos(&r#if.then_block.lock(), gotos);
                                collect_gotos(&r#if.else_block.lock(), gotos);
                            }
                            ast::Statement::While(r#while) => {
                                collect_gotos(&r#while.block.lock(), gotos);
                            }
                            ast::Statement::Repeat(repeat) => {
                                collect_gotos(&repeat.block.lock(), gotos);
                            }
                            ast::Statement::NumericFor(numeric_for) => {
                                collect_gotos(&numeric_for.block.lock(), gotos);
                            }
                            ast::Statement::GenericFor(generic_for) => {
                                collect_gotos(&generic_for.block.lock(), gotos);
                            }
                            _ => {}
                        }
                    }
                }

                let block = self.function.remove_block(node).unwrap();
                let mut goto_destinations = FxHashSet::default();
                collect_gotos(&block, &mut goto_destinations);
                for label in goto_destinations {
                    // TODO: block might have been merged/structured into another, output that block instead
                    // will require collecting label definitions in addition to references (gotos)
                    let target_node = self.label_to_node[&label];
                    if self.function.has_block(target_node) {
                        stack.push(target_node);
                    }
                }
                if let Some(ast::Statement::Goto(goto)) = res_block.last()
                // TODO: keep label -> block map instead
                    && goto.0.0[1..] == node.index().to_string()
                {
                    res_block.pop();
                }
                if !block
                    .first()
                    .is_some_and(|s| matches!(s, ast::Statement::Label(_)))
                {
                    res_block.push(ast::Comment::new(format!("block {}", node.index())).into());
                }
                res_block.extend(block.0)
            }
            // TODO: these nodes are never executed (i think), comment them out or dont include them
            for node in self.function.graph().node_indices().collect::<Vec<_>>() {
                let block = self.function.remove_block(node).unwrap();
                if !block
                    .first()
                    .is_some_and(|s| matches!(s, ast::Statement::Label(_)))
                {
                    res_block.push(ast::Comment::new(format!("block {}", node.index())).into());
                }
                res_block.extend(block.0)
            }

            res_block
        } else {
            Self::remove_last_return(
                self.function
                    .remove_block(self.function.entry().unwrap())
                    .unwrap(),
            )
        }
    }
}

pub fn lift(function: cfg::function::Function) -> ast::Block {
    GraphStructurer::new(function).structure()
}

#[cfg(test)]
mod tests {
    use ast::{Local, RcLocal};
    use cfg::{
        block::{BlockEdge, BranchType},
        function::Function,
    };

    use super::lift;

    fn call(name: &str) -> ast::Statement {
        ast::Call::new(ast::Global::from(name).into(), Vec::new()).into()
    }

    fn add_condition(
        function: &mut Function,
        node: petgraph::stable_graph::NodeIndex,
        condition: &str,
        then_node: petgraph::stable_graph::NodeIndex,
        else_node: petgraph::stable_graph::NodeIndex,
    ) {
        function.block_mut(node).unwrap().push(
            ast::If::new(
                ast::Global::from(condition).into(),
                ast::Block::default(),
                ast::Block::default(),
            )
            .into(),
        );
        function.set_edges(
            node,
            vec![
                (then_node, BlockEdge::new(BranchType::Then)),
                (else_node, BlockEdge::new(BranchType::Else)),
            ],
        );
    }

    fn assert_structured(output: &str) {
        assert!(!output.contains("goto "), "unexpected goto:\n{output}");
        assert!(!output.contains("::l"), "unexpected label:\n{output}");
        assert!(
            !output.contains("NumForInit"),
            "unlowered loop marker:\n{output}"
        );
        assert!(
            !output.contains("NumForNext"),
            "unlowered loop marker:\n{output}"
        );
    }

    #[test]
    fn folds_terminal_branch_into_guard() {
        let mut function = Function::new(0);
        let entry = function.new_block();
        let terminal = function.new_block();
        let continuation = function.new_block();
        let exit = function.new_block();
        function.set_entry(entry);

        add_condition(&mut function, entry, "shouldReturn", terminal, continuation);
        function
            .block_mut(terminal)
            .unwrap()
            .push(ast::Return::new(vec![ast::Literal::Number(7.0).into()]).into());
        function.block_mut(continuation).unwrap().push(call("work"));
        function.set_edges(
            continuation,
            vec![(exit, BlockEdge::new(BranchType::Unconditional))],
        );
        function
            .block_mut(exit)
            .unwrap()
            .push(ast::Return::default().into());

        let output = lift(function).to_string();
        assert_structured(&output);
        assert!(output.contains("if shouldReturn then"), "{output}");
        assert!(output.contains("return 7"), "{output}");
        assert!(output.contains("work()"), "{output}");
    }

    #[test]
    fn collapses_conditional_latch_into_repeat_until() {
        let mut function = Function::new(0);
        let header = function.new_block();
        let latch = function.new_block();
        let exit = function.new_block();
        function.set_entry(header);

        function.block_mut(header).unwrap().push(call("tick"));
        function.set_edges(
            header,
            vec![(latch, BlockEdge::new(BranchType::Unconditional))],
        );
        add_condition(&mut function, latch, "finished", exit, header);
        function.block_mut(exit).unwrap().push(call("done"));

        let output = lift(function).to_string();
        assert_structured(&output);
        assert!(output.contains("repeat"), "{output}");
        assert!(output.contains("until finished"), "{output}");
        assert!(output.contains("tick()"), "{output}");
        assert!(output.contains("done()"), "{output}");
    }

    #[test]
    fn structures_numeric_for_body_entry_continue() {
        let mut function = Function::new(0);
        let init = function.new_block();
        let header = function.new_block();
        let body = function.new_block();
        let work = function.new_block();
        let exit = function.new_block();
        function.set_entry(init);

        let counter = RcLocal::new(Local::new(Some("index".to_owned())));
        let limit = RcLocal::new(Local::new(Some("limit".to_owned())));
        let step = RcLocal::new(Local::new(Some("step".to_owned())));
        function
            .block_mut(init)
            .unwrap()
            .push(ast::NumForInit::new(counter.clone(), limit.clone(), step.clone()).into());
        function.set_edges(
            init,
            vec![(header, BlockEdge::new(BranchType::Unconditional))],
        );
        function
            .block_mut(header)
            .unwrap()
            .push(ast::NumForNext::new(counter, limit.into(), step.into()).into());
        function.set_edges(
            header,
            vec![
                (body, BlockEdge::new(BranchType::Then)),
                (exit, BlockEdge::new(BranchType::Else)),
            ],
        );
        add_condition(&mut function, body, "skip", header, work);
        function.block_mut(work).unwrap().push(call("process"));
        function.set_edges(
            work,
            vec![(header, BlockEdge::new(BranchType::Unconditional))],
        );
        function.block_mut(exit).unwrap().push(call("done"));

        let output = lift(function).to_string();
        assert_structured(&output);
        assert!(output.contains("for index ="), "{output}");
        assert!(
            output.contains("continue") || output.contains("if not skip then"),
            "{output}"
        );
        assert!(output.contains("process()"), "{output}");
        assert!(output.contains("done()"), "{output}");
    }
}
