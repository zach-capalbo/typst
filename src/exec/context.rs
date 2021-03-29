use std::mem;

use super::{Exec, FontFamily, State};
use crate::diag::{Diag, DiagSet, Pass};
use crate::env::Env;
use crate::eval::TemplateValue;
use crate::geom::{Align, Dir, Gen, GenAxis, Length, Linear, Sides, Size};
use crate::layout::{
    AnyNode, PadNode, PageRun, ParChild, ParNode, StackChild, StackNode, TextNode, Tree,
};
use crate::parse::{is_newline, Scanner};
use crate::syntax::Span;

/// The context for execution.
pub struct ExecContext<'a> {
    /// The environment from which resources are gathered.
    pub env: &'a mut Env,
    /// The active execution state.
    pub state: State,
    /// Execution diagnostics.
    pub diags: DiagSet,
    /// The tree of finished page runs.
    tree: Tree,
    /// When we are building the top-level stack, this contains metrics of the
    /// page. While building a group stack through `exec_group`, this is `None`.
    page: Option<PageBuilder>,
    /// The currently built stack of paragraphs.
    stack: StackBuilder,
}

impl<'a> ExecContext<'a> {
    /// Create a new execution context with a base state.
    pub fn new(env: &'a mut Env, state: State) -> Self {
        Self {
            env,
            diags: DiagSet::new(),
            tree: Tree { runs: vec![] },
            page: Some(PageBuilder::new(&state, true)),
            stack: StackBuilder::new(&state),
            state,
        }
    }

    /// Add a diagnostic.
    pub fn diag(&mut self, diag: Diag) {
        self.diags.insert(diag);
    }

    /// Set the font to monospace.
    pub fn set_monospace(&mut self) {
        let families = self.state.font.families_mut();
        families.list.insert(0, FontFamily::Monospace);
    }

    /// Execute a template and return the result as a stack node.
    pub fn exec_group(&mut self, template: &TemplateValue) -> StackNode {
        let snapshot = self.state.clone();
        let page = self.page.take();
        let stack = mem::replace(&mut self.stack, StackBuilder::new(&self.state));

        template.exec(self);

        self.state = snapshot;
        self.page = page;
        mem::replace(&mut self.stack, stack).build()
    }

    /// Push any node into the active paragraph.
    pub fn push(&mut self, node: impl Into<AnyNode>) {
        let align = self.state.aligns.cross;
        self.stack.par.push(ParChild::Any(node.into(), align));
    }

    /// Push a word space into the active paragraph.
    pub fn push_word_space(&mut self) {
        let em = self.state.font.resolve_size();
        let amount = self.state.par.word_spacing.resolve(em);
        self.stack.par.push_soft(ParChild::Spacing(amount));
    }

    /// Push text into the active paragraph.
    ///
    /// The text is split into lines at newlines.
    pub fn push_text(&mut self, text: &str) {
        let mut scanner = Scanner::new(text);
        let mut text = String::new();

        while let Some(c) = scanner.eat_merging_crlf() {
            if is_newline(c) {
                self.stack.par.push_text(mem::take(&mut text), &self.state);
                self.linebreak();
            } else {
                text.push(c);
            }
        }

        self.stack.par.push_text(text, &self.state);
    }

    /// Push spacing into paragraph or stack depending on `axis`.
    pub fn push_spacing(&mut self, axis: GenAxis, amount: Length) {
        match axis {
            GenAxis::Main => {
                self.stack.parbreak(&self.state);
                self.stack.push_hard(StackChild::Spacing(amount));
            }
            GenAxis::Cross => {
                self.stack.par.push_hard(ParChild::Spacing(amount));
            }
        }
    }

    /// Apply a forced line break.
    pub fn linebreak(&mut self) {
        self.stack.par.push_hard(ParChild::Linebreak);
    }

    /// Apply a forced paragraph break.
    pub fn parbreak(&mut self) {
        let em = self.state.font.resolve_size();
        let amount = self.state.par.spacing.resolve(em);
        self.stack.parbreak(&self.state);
        self.stack.push_soft(StackChild::Spacing(amount));
    }

    /// Apply a forced page break.
    pub fn pagebreak(&mut self, keep: bool, hard: bool, source: Span) {
        if let Some(builder) = &mut self.page {
            let page = mem::replace(builder, PageBuilder::new(&self.state, hard));
            let stack = mem::replace(&mut self.stack, StackBuilder::new(&self.state));
            self.tree.runs.extend(page.build(stack.build(), keep));
        } else {
            self.diag(error!(source, "cannot modify page from here"));
        }
    }

    /// Finish execution and return the created layout tree.
    pub fn finish(mut self) -> Pass<Tree> {
        assert!(self.page.is_some());
        self.pagebreak(true, false, Span::default());
        Pass::new(self.tree, self.diags)
    }
}

struct PageBuilder {
    size: Size,
    padding: Sides<Linear>,
    hard: bool,
}

impl PageBuilder {
    fn new(state: &State, hard: bool) -> Self {
        Self {
            size: state.page.size,
            padding: state.page.margins(),
            hard,
        }
    }

    fn build(self, child: StackNode, keep: bool) -> Option<PageRun> {
        let Self { size, padding, hard } = self;
        (!child.children.is_empty() || (keep && hard)).then(|| PageRun {
            size,
            child: PadNode { padding, child: child.into() }.into(),
        })
    }
}

struct StackBuilder {
    dirs: Gen<Dir>,
    children: Vec<StackChild>,
    last: Last<StackChild>,
    par: ParBuilder,
}

impl StackBuilder {
    fn new(state: &State) -> Self {
        Self {
            dirs: Gen::new(Dir::TTB, state.lang.dir),
            children: vec![],
            last: Last::None,
            par: ParBuilder::new(state),
        }
    }

    fn push_soft(&mut self, child: StackChild) {
        self.last.soft(child);
    }

    fn push_hard(&mut self, child: StackChild) {
        self.last.hard();
        self.children.push(child);
    }

    fn parbreak(&mut self, state: &State) {
        let par = mem::replace(&mut self.par, ParBuilder::new(state));
        if let Some(par) = par.build() {
            self.children.extend(self.last.any());
            self.children.push(par);
        }
    }

    fn build(self) -> StackNode {
        let Self { dirs, mut children, par, mut last } = self;
        if let Some(par) = par.build() {
            children.extend(last.any());
            children.push(par);
        }
        StackNode { dirs, children }
    }
}

struct ParBuilder {
    aligns: Gen<Align>,
    dir: Dir,
    line_spacing: Length,
    children: Vec<ParChild>,
    last: Last<ParChild>,
}

impl ParBuilder {
    fn new(state: &State) -> Self {
        let em = state.font.resolve_size();
        Self {
            aligns: state.aligns,
            dir: state.lang.dir,
            line_spacing: state.par.leading.resolve(em),
            children: vec![],
            last: Last::None,
        }
    }

    fn push(&mut self, child: ParChild) {
        self.children.extend(self.last.any());
        self.children.push(child);
    }

    fn push_text(&mut self, text: String, state: &State) {
        self.children.extend(self.last.any());

        let align = state.aligns.cross;
        let props = state.font.resolve_props();

        if let Some(ParChild::Text(prev, prev_align)) = self.children.last_mut() {
            if *prev_align == align && prev.props == props {
                prev.text.push_str(&text);
                return;
            }
        }

        self.children.push(ParChild::Text(TextNode { text, props }, align));
    }

    fn push_soft(&mut self, child: ParChild) {
        self.last.soft(child);
    }

    fn push_hard(&mut self, child: ParChild) {
        self.last.hard();
        self.children.push(child);
    }

    fn build(self) -> Option<StackChild> {
        let Self { aligns, dir, line_spacing, children, .. } = self;
        (!children.is_empty()).then(|| {
            let node = ParNode { dir, line_spacing, children };
            StackChild::Any(node.into(), aligns)
        })
    }
}

/// Finite state machine for spacing coalescing.
enum Last<N> {
    None,
    Any,
    Soft(N),
}

impl<N> Last<N> {
    fn any(&mut self) -> Option<N> {
        match mem::replace(self, Self::Any) {
            Self::Soft(soft) => Some(soft),
            _ => None,
        }
    }

    fn soft(&mut self, soft: N) {
        if let Self::Any = self {
            *self = Self::Soft(soft);
        }
    }

    fn hard(&mut self) {
        *self = Self::None;
    }
}