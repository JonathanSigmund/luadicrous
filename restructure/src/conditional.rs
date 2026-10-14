use ast::Reduce;
use cfg::block::{BlockEdge, BranchType};
use itertools::Itertools;
use parking_lot::Mutex;
use petgraph::visit::EdgeRef;
use triomphe::Arc;
use tuple::Map;

use crate::GraphStructurer;
use petgraph::{algo::dominators::Dominators, stable_graph::NodeIndex};

const MAX_DUPLICATED_BRANCH_STATEMENTS: usize = 48;
const MAX_DUPLICATED_STATEMENTS_PER_FUNCTION: usize = 256;

fn structured_statement_count(block: &ast::Block) -> usize {
    block
        .iter()
        .map(|statement| {
            1 + match statement {
                ast::Statement::If(value) => {
                    structured_statement_count(&value.then_block.lock())
                        + structured_statement_count(&value.else_block.lock())
                }
                ast::Statement::While(value) => structured_statement_count(&value.block.lock()),
                ast::Statement::Repeat(value) => structured_statement_count(&value.block.lock()),
                ast::Statement::NumericFor(value) => {
                    structured_statement_count(&value.block.lock())
                }
                ast::Statement::GenericFor(value) => {
                    structured_statement_count(&value.block.lock())
                }
                _ => 0,
            }
        })
        .sum()
}

fn contains_internal_for_marker(block: &ast::Block) -> bool {
    block.iter().any(|statement| match statement {
        ast::Statement::NumForInit(_)
        | ast::Statement::NumForNext(_)
        | ast::Statement::GenericForInit(_)
        | ast::Statement::GenericForNext(_) => true,
        ast::Statement::If(value) => {
            contains_internal_for_marker(&value.then_block.lock())
                || contains_internal_for_marker(&value.else_block.lock())
        }
        ast::Statement::While(value) => contains_internal_for_marker(&value.block.lock()),
        ast::Statement::Repeat(value) => contains_internal_for_marker(&value.block.lock()),
        ast::Statement::NumericFor(value) => contains_internal_for_marker(&value.block.lock()),
        ast::Statement::GenericFor(value) => contains_internal_for_marker(&value.block.lock()),
        _ => false,
    })
}

fn deep_clone_block(block: &ast::Block) -> ast::Block {
    ast::Block(block.iter().map(deep_clone_statement).collect())
}

fn deep_clone_statement(statement: &ast::Statement) -> ast::Statement {
    let mut cloned = statement.clone();
    match &mut cloned {
        ast::Statement::If(value) => {
            let then_block = {
                let block = value.then_block.lock();
                deep_clone_block(&block)
            };
            let else_block = {
                let block = value.else_block.lock();
                deep_clone_block(&block)
            };
            value.then_block = Arc::new(Mutex::new(then_block));
            value.else_block = Arc::new(Mutex::new(else_block));
        }
        ast::Statement::While(value) => {
            let block = {
                let block = value.block.lock();
                deep_clone_block(&block)
            };
            value.block = Arc::new(Mutex::new(block));
        }
        ast::Statement::Repeat(value) => {
            let block = {
                let block = value.block.lock();
                deep_clone_block(&block)
            };
            value.block = Arc::new(Mutex::new(block));
        }
        ast::Statement::NumericFor(value) => {
            let block = {
                let block = value.block.lock();
                deep_clone_block(&block)
            };
            value.block = Arc::new(Mutex::new(block));
        }
        ast::Statement::GenericFor(value) => {
            let block = {
                let block = value.block.lock();
                deep_clone_block(&block)
            };
            value.block = Arc::new(Mutex::new(block));
        }
        _ => {}
    }
    cloned
}

impl GraphStructurer {
    fn simplify_if(if_stat: &mut ast::If) {
        if let Some(unary) = if_stat.condition.as_unary() {
            if unary.operation == ast::UnaryOperation::Not {
                if_stat.condition = *unary.value.clone();
                std::mem::swap(&mut if_stat.then_block, &mut if_stat.else_block);
            }
        }
    }

    fn expand_if(if_stat: &mut ast::If) -> Option<ast::Block> {
        let mut then_block = if_stat.then_block.lock();
        let mut else_block = if_stat.else_block.lock();
        let then_return = then_block.last().and_then(|x| x.as_return());
        let else_return = else_block.last().and_then(|x| x.as_return());
        if let Some(then_return) = then_return
            && let Some(else_return) = else_return
        {
            if then_return.values.is_empty() && else_return.values.is_empty() {
                then_block.pop();
                else_block.pop();
                None
            } else if !then_return.values.is_empty() && else_return.values.is_empty() {
                Some(std::mem::take(&mut else_block))
            } else if then_return.values.is_empty() && !else_return.values.is_empty() {
                let then_block = std::mem::replace::<ast::Block>(
                    &mut then_block,
                    std::mem::take(&mut else_block),
                );
                // TODO: unnecessary clone (also other cases)
                if_stat.condition =
                    ast::Unary::new(if_stat.condition.clone(), ast::UnaryOperation::Not)
                        .reduce_condition();
                Some(then_block)
            } else {
                match then_block.len().cmp(&else_block.len()) {
                    std::cmp::Ordering::Less => Some(std::mem::take(&mut else_block)),
                    std::cmp::Ordering::Greater => {
                        let then_block = std::mem::replace::<ast::Block>(
                            &mut then_block,
                            std::mem::take(&mut else_block),
                        );
                        if_stat.condition =
                            ast::Unary::new(if_stat.condition.clone(), ast::UnaryOperation::Not)
                                .reduce_condition();
                        Some(then_block)
                    }
                    // TODO: `Some(std::mem::take(&mut if_stat.else_block))`?
                    std::cmp::Ordering::Equal => None,
                }
            }
        } else {
            None
        }
    }

    pub(crate) fn try_collapse_terminal_conditional(&mut self, entry: NodeIndex) -> bool {
        if self
            .function
            .block(entry)
            .and_then(|block| block.last())
            .and_then(ast::Statement::as_if)
            .is_none()
        {
            return false;
        }
        let Some((then_edge, else_edge)) = self.function.conditional_edges(entry) else {
            return false;
        };
        let then_node = then_edge.target();
        let else_node = else_edge.target();
        let is_terminal = |node| {
            self.function.successor_blocks(node).next().is_none()
                && self.function.predecessor_blocks(node).count() == 1
                && self
                    .function
                    .block(node)
                    .and_then(|block| block.last())
                    .is_some_and(|statement| matches!(statement, ast::Statement::Return(_)))
        };
        let (terminal, continuation, terminal_is_then) =
            match (is_terminal(then_node), is_terminal(else_node)) {
                (true, false) => (then_node, else_node, true),
                (false, true) => (else_node, then_node, false),
                _ => return false,
            };

        let terminal_block = self.function.remove_block(terminal).unwrap();
        let block = self.function.block_mut(entry).unwrap();
        let Some(if_statement) = block.last_mut().and_then(ast::Statement::as_if_mut) else {
            return false;
        };
        if terminal_is_then {
            if_statement.then_block = Arc::new(Mutex::new(terminal_block));
        } else {
            if_statement.else_block = Arc::new(Mutex::new(terminal_block));
        }
        Self::simplify_if(if_statement);
        self.function.set_edges(
            entry,
            vec![(continuation, BlockEdge::new(BranchType::Unconditional))],
        );
        self.match_jump(entry, Some(continuation));
        true
    }

    // a -> b -> d + a -> c -> d
    // results in a -> d
    fn match_diamond_conditional(
        &mut self,
        entry: NodeIndex,
        then_node: NodeIndex,
        else_node: NodeIndex,
    ) -> bool {
        let mut then_successors = self.function.successor_blocks(then_node).collect_vec();
        let mut else_successors = self.function.successor_blocks(else_node).collect_vec();

        if then_successors.len() > 1 || else_successors.len() > 1 {
            if self.is_loop_header(entry) {
                // TODO: ugly
                let t = if let Some(index) = then_successors.iter().position(|n| *n == entry) {
                    then_successors.swap_remove(index);
                    true
                } else {
                    false
                };
                let e = if let Some(index) = else_successors.iter().position(|n| *n == entry) {
                    else_successors.swap_remove(index);
                    true
                } else {
                    false
                };
                if (!t && then_successors.len() != 1) || (!e && else_successors.len() != 1) {
                    return false;
                }

                if then_successors != else_successors {
                    return false;
                }

                let mut refine = |n| {
                    let (then_target, else_target) = self
                        .function
                        .conditional_edges(n)
                        .unwrap()
                        .map(|e| e.target());
                    let block = self.function.block_mut(n).unwrap();
                    if let Some(if_stat) = block.last_mut().unwrap().as_if_mut() {
                        if then_target == entry {
                            if_stat.then_block =
                                Arc::new(Mutex::new(vec![ast::Continue {}.into()].into()));
                            true
                        } else if else_target == entry {
                            if_stat.else_block =
                                Arc::new(Mutex::new(vec![ast::Continue {}.into()].into()));
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                let then_changed = if t { refine(then_node) } else { false };
                let else_changed = if e { refine(else_node) } else { false };
                if !then_changed && !else_changed {
                    return false;
                }
                if t && e {
                    assert!(then_changed && else_changed)
                }
            } else {
                return false;
            }
        } else if then_successors != else_successors {
            return false;
        }

        let then_shared = self.function.predecessor_blocks(then_node).count() != 1;
        let else_shared = self.function.predecessor_blocks(else_node).count() != 1;
        if (then_shared
            && (then_node == entry
                || self.is_loop_header(then_node)
                || self.is_for_next(then_node)
                || contains_internal_for_marker(self.function.block(then_node).unwrap())))
            || (else_shared
                && (else_node == entry
                    || self.is_loop_header(else_node)
                    || self.is_for_next(else_node)
                    || contains_internal_for_marker(self.function.block(else_node).unwrap())))
        {
            return false;
        }

        let then_duplicate_cost = then_shared
            .then(|| structured_statement_count(self.function.block(then_node).unwrap()))
            .unwrap_or(0);
        let else_duplicate_cost = else_shared
            .then(|| structured_statement_count(self.function.block(else_node).unwrap()))
            .unwrap_or(0);
        let duplicate_cost = then_duplicate_cost + else_duplicate_cost;
        if then_duplicate_cost > MAX_DUPLICATED_BRANCH_STATEMENTS
            || else_duplicate_cost > MAX_DUPLICATED_BRANCH_STATEMENTS
            || self.duplicated_statements + duplicate_cost > MAX_DUPLICATED_STATEMENTS_PER_FUNCTION
        {
            return false;
        }
        self.duplicated_statements += duplicate_cost;

        let then_block = if then_shared {
            deep_clone_block(self.function.block(then_node).unwrap())
        } else {
            self.function.remove_block(then_node).unwrap()
        };
        let else_block = if else_shared {
            deep_clone_block(self.function.block(else_node).unwrap())
        } else {
            self.function.remove_block(else_node).unwrap()
        };

        let block = self.function.block_mut(entry).unwrap();
        // TODO: STYLE: rename to r#if?
        let if_stat = block.last_mut().unwrap().as_if_mut().unwrap();
        if_stat.then_block = Arc::new(then_block.into());
        if_stat.else_block = Arc::new(else_block.into());
        Self::simplify_if(if_stat);

        let after = Self::expand_if(if_stat);
        if if_stat.then_block.lock().is_empty() {
            // TODO: unnecessary clone
            if_stat.condition =
                ast::Unary::new(if_stat.condition.clone(), ast::UnaryOperation::Not)
                    .reduce_condition();
            std::mem::swap(&mut if_stat.then_block, &mut if_stat.else_block);
        }
        if let Some(after) = after {
            block.extend(after.0);
        }

        let exit = then_successors.first().cloned();
        if let Some(exit) = exit {
            self.function.set_edges(
                entry,
                vec![(exit, BlockEdge::new(BranchType::Unconditional))],
            );
        } else {
            self.function.remove_edges(entry);
        }
        self.match_jump(entry, exit);

        true
    }

    // a -> b -> c + a -> c
    // results in a -> c
    fn match_triangle_conditional(
        &mut self,
        entry: NodeIndex,
        then_node: NodeIndex,
        else_node: NodeIndex,
    ) -> bool {
        let mut _match_triangle_conditional = |then_node, else_node, inverted| {
            let then_successors = self.function.successor_blocks(then_node).collect_vec();

            if then_successors.len() > 1 {
                return false;
            }

            let then_shared = self.function.predecessor_blocks(then_node).count() != 1;
            if then_shared
                && (then_node == entry
                    || self.is_loop_header(then_node)
                    || self.is_for_next(then_node)
                    || contains_internal_for_marker(self.function.block(then_node).unwrap()))
            {
                return false;
            }

            let duplicate_cost = then_shared
                .then(|| structured_statement_count(self.function.block(then_node).unwrap()))
                .unwrap_or(0);
            if duplicate_cost > MAX_DUPLICATED_BRANCH_STATEMENTS
                || self.duplicated_statements + duplicate_cost
                    > MAX_DUPLICATED_STATEMENTS_PER_FUNCTION
            {
                return false;
            }
            self.duplicated_statements += duplicate_cost;

            if !then_successors.is_empty() && then_successors[0] != else_node {
                return false;
            }

            let then_block = if then_shared {
                deep_clone_block(self.function.block(then_node).unwrap())
            } else {
                self.function.remove_block(then_node).unwrap()
            };

            let block = self.function.block_mut(entry).unwrap();
            let if_stat = block.last_mut().unwrap().as_if_mut().unwrap();
            if_stat.then_block = Arc::new(then_block.into());

            if inverted {
                if_stat.condition =
                    ast::Unary::new(if_stat.condition.clone(), ast::UnaryOperation::Not)
                        .reduce_condition()
            }

            //Self::simplify_if(if_stat);

            self.function.set_edges(
                entry,
                vec![(else_node, BlockEdge::new(BranchType::Unconditional))],
            );

            self.match_jump(entry, Some(else_node));

            true
        };

        _match_triangle_conditional(then_node, else_node, false)
            || _match_triangle_conditional(else_node, then_node, true)
    }

    // a -> b a -> c
    pub(crate) fn refine_virtual_edge_jump(
        &mut self,
        post_dom: &Dominators<NodeIndex>,
        entry: NodeIndex,
        node: NodeIndex,
        header: NodeIndex,
        next: Option<NodeIndex>,
    ) -> bool {
        if node == header {
            // TODO: only check back edges?
            if !self
                .function
                .predecessor_blocks(header)
                .filter(|&n| n != entry)
                .any(|n| {
                    post_dom
                        .dominators(entry)
                        .is_some_and(|mut p| p.contains(&n))
                })
            {
                return false;
            }
            let block = &mut self.function.block_mut(entry).unwrap();
            block.push(ast::Continue {}.into());
        } else if Some(node) == next {
            let block = &mut self.function.block_mut(entry).unwrap();
            block.push(ast::Break {}.into());
        }
        self.function.set_edges(entry, vec![]);
        true
    }

    pub(crate) fn refine_virtual_edge_conditional(
        &mut self,
        post_dom: &Dominators<NodeIndex>,
        entry: NodeIndex,
        then_node: NodeIndex,
        else_node: NodeIndex,
        header: NodeIndex,
        next: Option<NodeIndex>,
    ) -> bool {
        let then_main_cont = self
            .function
            .predecessor_blocks(header)
            .filter(|&n| n != entry)
            .any(|n| {
                post_dom
                    .dominators(then_node)
                    .is_some_and(|mut p| p.contains(&n))
            });

        let else_main_cont = self
            .function
            .predecessor_blocks(header)
            .filter(|&n| n != entry)
            .any(|n| {
                post_dom
                    .dominators(else_node)
                    .is_some_and(|mut p| p.contains(&n))
            });

        let mut changed = false;
        let header_successors = self.function.successor_blocks(header).collect_vec();
        let is_for_body_entry = self.is_for_next(header) && header_successors.contains(&entry);
        let block = self.function.block_mut(entry).unwrap();
        if let Some(if_stat) = block.last_mut().unwrap().as_if_mut() {
            if then_node == header
                && ((!header_successors.contains(&entry) && then_main_cont) || is_for_body_entry)
            {
                if_stat.then_block = Arc::new(Mutex::new(vec![ast::Continue {}.into()].into()));
                changed = true;
            } else if Some(then_node) == next {
                if_stat.then_block = Arc::new(Mutex::new(vec![ast::Break {}.into()].into()));
                changed = true;
            }
            if else_node == header
                && ((!header_successors.contains(&entry) && else_main_cont) || is_for_body_entry)
            {
                if_stat.else_block = Arc::new(Mutex::new(vec![ast::Continue {}.into()].into()));
                changed = true;
            } else if Some(else_node) == next {
                if_stat.else_block = Arc::new(Mutex::new(vec![ast::Break {}.into()].into()));
                changed = true;
            }
            if !if_stat.then_block.lock().is_empty() && if_stat.else_block.lock().is_empty() {
                self.function.set_edges(
                    entry,
                    vec![(else_node, BlockEdge::new(BranchType::Unconditional))],
                );
                changed = true;
            } else if if_stat.then_block.lock().is_empty() && !if_stat.else_block.lock().is_empty()
            {
                if_stat.condition =
                    ast::Unary::new(if_stat.condition.clone(), ast::UnaryOperation::Not)
                        .reduce_condition();
                std::mem::swap(&mut if_stat.then_block, &mut if_stat.else_block);
                self.function.set_edges(
                    entry,
                    vec![(then_node, BlockEdge::new(BranchType::Unconditional))],
                );
                changed = true;
            } else if !if_stat.then_block.lock().is_empty() && !if_stat.else_block.lock().is_empty()
            {
                self.function.remove_edges(entry);
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn match_conditional(
        &mut self,
        entry: NodeIndex,
        then_node: NodeIndex,
        else_node: NodeIndex,
    ) -> bool {
        let is_canonical_loop_latch = self.function.predecessor_blocks(entry).any(|header| {
            self.is_loop_header(header)
                && self.function.successor_blocks(header).exactly_one().ok() == Some(entry)
                && (then_node == header || else_node == header)
        });
        if is_canonical_loop_latch {
            return false;
        }

        let block = self.function.block_mut(entry).unwrap();
        if block.last_mut().unwrap().as_if_mut().is_none() {
            // for loops
            return false;
        }

        self.match_diamond_conditional(entry, then_node, else_node)
            || self.match_triangle_conditional(entry, then_node, else_node)
    }
}
