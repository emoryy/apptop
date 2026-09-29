use std::collections::HashSet;
use std::fs;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::Sampler;
use crate::config::Config;
use crate::history::History;
use crate::i18n::{count, tr};
use crate::kill::{self, OTHER, Plan, Sig};
use crate::model::{Kind, Model, Node, Summary, Target};

const AMBER: Color = Color::Rgb(242, 184, 75);
const CORAL: Color = Color::Rgb(255, 122, 89);
const DIM: Color = Color::Rgb(150, 160, 175);
const HEADER_BG: Color = Color::Rgb(42, 49, 64);
const HEADER_FG: Color = Color::Rgb(230, 234, 240);
const SEL_BG: Color = Color::Rgb(59, 74, 107);
const GOOD: Color = Color::Rgb(127, 211, 160);
const TRACK: Color = Color::Rgb(70, 78, 92);
const GAP: u16 = 2;
/// Optional columns, least important first.
const DROP_ORDER: [ViewCol; 9] = [
    ViewCol::Procs,
    ViewCol::Cache,
    ViewCol::Psi,
    ViewCol::Io,
    ViewCol::Gpu,
    ViewCol::Share,
    ViewCol::Delta,
    ViewCol::Vram,
    ViewCol::Swap,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Col {
    Name,
    Mem,
    Delta,
    Swap,
    Psi,
    Cache,
    Vram,
    Gpu,
    Cpu,
    Io,
    Procs,
}

impl Col {
    const ALL: [Col; 11] = [
        Col::Mem,
        Col::Delta,
        Col::Swap,
        Col::Psi,
        Col::Cache,
        Col::Vram,
        Col::Gpu,
        Col::Cpu,
        Col::Io,
        Col::Procs,
        Col::Name,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Col::Name => "name",
            Col::Mem => "mem",
            Col::Delta => "delta",
            Col::Swap => "swap",
            Col::Psi => "psi",
            Col::Cache => "cache",
            Col::Vram => "vram",
            Col::Gpu => "gpu",
            Col::Cpu => "cpu",
            Col::Io => "io",
            Col::Procs => "procs",
        }
    }

    pub fn from_name(s: &str) -> Option<Col> {
        Col::ALL.into_iter().find(|c| c.name() == s)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ViewCol {
    Mem,
    Share,
    Delta,
    Swap,
    Psi,
    Cpu,
    Gpu,
    Vram,
    Io,
    Cache,
    Procs,
    Name,
}

impl ViewCol {
    fn title(self) -> &'static str {
        match self {
            ViewCol::Mem => tr("Memory", "Memória"),
            ViewCol::Share => tr("Share", "Arány"),
            ViewCol::Delta => tr("Δ5m", "Δ5p"),
            ViewCol::Swap => "Swap",
            ViewCol::Psi => tr("Pressure", "Nyomás"),
            ViewCol::Cpu => "CPU%",
            ViewCol::Gpu => "GPU%",
            ViewCol::Vram => "VRAM",
            ViewCol::Io => tr("Disk/s", "Lemez/s"),
            ViewCol::Cache => "Cache",
            ViewCol::Procs => "Proc",
            ViewCol::Name => tr("Program", "Program"),
        }
    }

    /// Room for the widest value and for the title with its sort arrow.
    fn width(self) -> u16 {
        match self {
            ViewCol::Share => 12,
            ViewCol::Name => 0,
            c => (c.title().width() as u16 + 1).max(6),
        }
    }

    fn sort_col(self) -> Option<Col> {
        Some(match self {
            ViewCol::Mem => Col::Mem,
            ViewCol::Share => return None,
            ViewCol::Delta => Col::Delta,
            ViewCol::Swap => Col::Swap,
            ViewCol::Psi => Col::Psi,
            ViewCol::Cpu => Col::Cpu,
            ViewCol::Gpu => Col::Gpu,
            ViewCol::Vram => Col::Vram,
            ViewCol::Io => Col::Io,
            ViewCol::Cache => Col::Cache,
            ViewCol::Procs => Col::Procs,
            ViewCol::Name => Col::Name,
        })
    }
}

pub fn fmt_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if b == 0 {
        "0".into()
    } else if f < K * K {
        format!("{:.0}K", f / K)
    } else if f < K * K * K {
        format!("{:.0}M", f / K / K)
    } else if f < 100.0 * K * K * K {
        format!("{:.1}G", f / K / K / K)
    } else {
        format!("{:.0}G", f / K / K / K)
    }
}

fn fmt_delta(d: i64) -> String {
    let sign = if d < 0 { "-" } else { "+" };
    format!("{sign}{}", fmt_bytes(d.unsigned_abs()))
}

pub fn display_name(n: &Node) -> String {
    if n.count > 1 && n.kind != Kind::Rest {
        format!("{} ×{}", n.name, n.count)
    } else {
        n.name.clone()
    }
}

fn value(n: &Node, col: Col) -> f64 {
    match col {
        Col::Name | Col::Mem => n.mem as f64,
        Col::Delta => n.delta.map(|d| d.0 as f64).unwrap_or(0.0),
        Col::Swap => n.swap as f64,
        Col::Psi => n.psi.unwrap_or(0.0),
        Col::Cache => n.cache.unwrap_or(0) as f64,
        Col::Vram => n.vram as f64,
        Col::Gpu => n.gpu,
        Col::Cpu => n.cpu,
        Col::Io => n.io_read + n.io_write,
        Col::Procs => n.procs as f64,
    }
}

pub fn sort_tree(nodes: &mut [Node], col: Col, desc: bool) {
    nodes.sort_by(|a, b| {
        let rest = (a.kind == Kind::Rest).cmp(&(b.kind == Kind::Rest));
        if rest.is_ne() {
            return rest;
        }
        let o = match col {
            Col::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            _ => value(a, col).total_cmp(&value(b, col)),
        };
        let o = if desc { o.reverse() } else { o };
        o.then_with(|| b.mem.cmp(&a.mem)).then_with(|| a.name.cmp(&b.name))
    });
    for n in nodes {
        sort_tree(&mut n.children, col, desc);
    }
}

pub fn dump(model: &Model, depth: usize) {
    let mut roots = model.roots.clone();
    sort_tree(&mut roots, Col::Mem, true);
    let s = &model.summary;
    print!(
        "CPU {:.0}%  RAM {}/{}  swap {}/{} (in {}/s out {}/s)",
        s.cpu_pct,
        fmt_bytes(s.mem_used),
        fmt_bytes(s.mem_total),
        fmt_bytes(s.swap_used),
        fmt_bytes(s.swap_total),
        fmt_bytes(s.swap_in as u64),
        fmt_bytes(s.swap_out as u64)
    );
    if let Some(g) = &s.gpu {
        print!("  GPU {}%  VRAM {}/{}", g.util, fmt_bytes(g.used), fmt_bytes(g.total));
    }
    println!();
    println!(
        "{:>7} {:>7} {:>6} {:>7} {:>6} {:>5} {:>6} {:>7} {:>5}  NAME",
        "MEM", "SWAP", "PSI", "CACHE", "VRAM", "GPU%", "CPU%", "IO/s", "PROC"
    );
    fn rec(n: &Node, level: usize, depth: usize) {
        println!(
            "{:>7} {:>7} {:>6} {:>7} {:>6} {:>5.0} {:>6.1} {:>7} {:>5}  {}{} {}",
            fmt_bytes(n.mem),
            fmt_bytes(n.swap),
            n.psi.map(|p| format!("{p:.1}")).unwrap_or_else(|| "-".into()),
            n.cache.map(fmt_bytes).unwrap_or_else(|| "-".into()),
            if n.vram > 0 { fmt_bytes(n.vram) } else { "-".into() },
            n.gpu,
            n.cpu,
            fmt_bytes((n.io_read + n.io_write) as u64),
            n.procs,
            "  ".repeat(level),
            display_name(n),
            n.detail
        );
        if level + 1 < depth {
            for c in &n.children {
                rec(c, level + 1, depth);
            }
        }
    }
    for r in &roots {
        rec(r, 0, depth);
    }
}

enum Cmd {
    SetSplit(bool),
}

struct Row {
    key: String,
    depth: usize,
    expandable: bool,
    expanded: bool,
    node: Node,
}

#[derive(Clone, Copy)]
enum KillHit {
    Sig(Sig),
    Menu,
    Pick(usize),
    Key(KeyCode),
}

enum Modal {
    None,
    Help,
    /// `other` remembers the last pick from the menu; `menu` is the open menu's cursor
    Kill {
        plan: Plan,
        sig: Sig,
        other: usize,
        menu: Option<usize>,
    },
}

/// Facts about the selected row read from /proc, refreshed once per model.
struct Info {
    key: String,
    generation: u64,
    main: Option<ProcInfo>,
}

struct ProcInfo {
    pid: u32,
    ppid: u32,
    user: String,
    age: Duration,
    cmdline: String,
    cwd: String,
}

struct App {
    model: Option<Model>,
    generation: u64,
    history: Arc<Mutex<History>>,
    rows: Vec<Row>,
    expanded: HashSet<String>,
    selected_key: Option<String>,
    selected: usize,
    offset: usize,
    cfg: Config,
    filter: String,
    filter_mode: bool,
    modal: Modal,
    gpu: bool,
    interval: Duration,
    header_cells: Vec<(ViewCol, u16, u16)>,
    /// footer hit segments: (from x, to x exclusive, key it stands for); KeyCode::Null toggles expand/collapse
    footer_cells: Vec<(u16, u16, KeyCode)>,
    footer_y: u16,
    /// the last pick from the kill panel's "other" menu, kept for the session
    last_other: usize,
    /// kill panel hit segments: (y, from x, to x, what)
    kill_cells: Vec<(u16, u16, u16, KillHit)>,
    row_actions: Vec<(u16, u16, KeyCode)>,
    row_actions_y: u16,
    name_x: u16,
    table_top: u16,
    table_height: u16,
    last_click: Option<(Instant, usize)>,
    cmd_tx: mpsc::Sender<Cmd>,
    msg_tx: mpsc::Sender<(String, bool)>,
    status: Option<(String, Instant, bool)>,
    info: Option<Info>,
    quit: bool,
}

pub fn run(sampler: Sampler, interval: Duration, cfg: Config) -> Result<()> {
    let gpu = sampler.gpu_available();
    let history = sampler.history.clone();
    let (model_tx, model_rx) = mpsc::channel::<Model>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<(String, bool)>();
    thread::spawn(move || sample_loop(sampler, interval, model_tx, cmd_rx));

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let mut app = App {
        model: None,
        generation: 0,
        history,
        rows: Vec::new(),
        expanded: HashSet::new(),
        selected_key: None,
        selected: 0,
        offset: 0,
        cfg,
        filter: String::new(),
        filter_mode: false,
        modal: Modal::None,
        gpu,
        interval,
        header_cells: Vec::new(),
        footer_cells: Vec::new(),
        footer_y: 0,
        last_other: 0,
        kill_cells: Vec::new(),
        row_actions: Vec::new(),
        row_actions_y: u16::MAX,
        name_x: 0,
        table_top: 0,
        table_height: 0,
        last_click: None,
        cmd_tx,
        msg_tx,
        status: None,
        info: None,
        quit: false,
    };
    let res = event_loop(&mut terminal, &mut app, &model_rx, &msg_rx);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if std::env::var_os("APPTOP_DEMO").is_none() {
        app.cfg.save();
    }
    res
}

fn sample_loop(mut sampler: Sampler, interval: Duration, tx: mpsc::Sender<Model>, rx: mpsc::Receiver<Cmd>) {
    // a short first interval so CPU numbers show up right away
    sampler.sample();
    let mut wait = Duration::from_millis(300);
    loop {
        match rx.recv_timeout(wait) {
            Ok(Cmd::SetSplit(v)) => sampler.split_terminals = v,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if tx.send(sampler.sample()).is_err() {
            return;
        }
        wait = interval;
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    model_rx: &mpsc::Receiver<Model>,
    msg_rx: &mpsc::Receiver<(String, bool)>,
) -> Result<()> {
    loop {
        let mut changed = false;
        while let Ok(m) = model_rx.try_recv() {
            app.model = Some(m);
            changed = true;
        }
        if changed {
            app.generation += 1;
            app.rebuild();
        }
        while let Ok((msg, err)) = msg_rx.try_recv() {
            app.status = Some((msg, Instant::now(), err));
        }
        terminal.draw(|f| app.draw(f.area(), f.buffer_mut()))?;
        if event::poll(Duration::from_millis(100))? {
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                    Event::Mouse(m) => app.on_mouse(m),
                    _ => {}
                }
                if app.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

fn matches(n: &Node, f: &str) -> bool {
    n.name.to_lowercase().contains(f) || n.detail.to_lowercase().contains(f)
}

fn subtree_matches(n: &Node, f: &str) -> bool {
    matches(n, f) || n.children.iter().any(|c| subtree_matches(c, f))
}

impl App {
    fn rebuild(&mut self) {
        let Some(model) = &self.model else { return };
        let mut roots = model.roots.clone();
        sort_tree(&mut roots, self.cfg.sort, self.cfg.desc);
        let filter = self.filter.to_lowercase();
        let mut rows = Vec::new();
        fn walk(n: &Node, depth: usize, filter: &str, expanded: &HashSet<String>, rows: &mut Vec<Row>) {
            if !filter.is_empty() && !subtree_matches(n, filter) {
                return;
            }
            // while filtering, open whatever leads to a match; below a match show everything
            let auto_open =
                !filter.is_empty() && !matches(n, filter) && n.children.iter().any(|c| subtree_matches(c, filter));
            let is_open = !n.children.is_empty() && (expanded.contains(&n.key) || auto_open);
            let filter = if matches(n, filter) { "" } else { filter };
            rows.push(Row {
                key: n.key.clone(),
                depth,
                expandable: !n.children.is_empty(),
                expanded: is_open,
                node: Node {
                    children: Vec::new(),
                    ..n.clone()
                },
            });
            if is_open {
                for c in &n.children {
                    walk(c, depth + 1, filter, expanded, rows);
                }
            }
        }
        for r in &roots {
            walk(r, 0, &filter, &self.expanded, &mut rows);
        }
        self.rows = rows;
        if let Some(k) = &self.selected_key
            && let Some(i) = self.rows.iter().position(|r| &r.key == k)
        {
            self.selected = i;
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.selected_key = None;
            return;
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        self.selected_key = Some(self.rows[self.selected].key.clone());
        let h = self.table_height.max(1) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + h {
            self.offset = self.selected + 1 - h;
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(h));
    }

    fn move_to(&mut self, i: isize) {
        let max = self.rows.len() as isize - 1;
        self.selected = i.clamp(0, max.max(0)) as usize;
        self.clamp_selection();
    }

    fn set_sort(&mut self, col: Col) {
        if self.cfg.sort == col {
            self.cfg.desc = !self.cfg.desc;
        } else {
            self.cfg.sort = col;
            self.cfg.desc = col != Col::Name;
        }
        self.rebuild();
    }

    fn cycle_sort(&mut self, step: isize) {
        let cols: Vec<Col> = self.columns(u16::MAX).iter().filter_map(|c| c.sort_col()).collect();
        let i = cols.iter().position(|c| *c == self.cfg.sort).unwrap_or(0) as isize;
        let next = cols[(i + step).rem_euclid(cols.len() as isize) as usize];
        self.cfg.sort = next;
        self.cfg.desc = next != Col::Name;
        self.rebuild();
    }

    fn toggle(&mut self, i: usize, open: Option<bool>) {
        let Some(r) = self.rows.get(i) else { return };
        if !r.expandable {
            return;
        }
        let want = open.unwrap_or(!r.expanded);
        if want {
            self.expanded.insert(r.key.clone());
        } else {
            self.expanded.remove(&r.key);
        }
        self.rebuild();
    }

    fn parent_of(&self, i: usize) -> Option<usize> {
        let d = self.rows.get(i)?.depth;
        (0..i).rev().find(|&j| self.rows[j].depth < d)
    }

    fn kill_key(&mut self, code: KeyCode) {
        let Modal::Kill { plan, sig, other, menu } = &mut self.modal else {
            return;
        };
        if let Some(cur) = menu {
            match code {
                KeyCode::Up => *cur = (*cur + OTHER.len() - 1) % OTHER.len(),
                KeyCode::Down => *cur = (*cur + 1) % OTHER.len(),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    *other = *cur;
                    *sig = Sig::Other(*cur);
                    *menu = None;
                    self.last_other = *other;
                }
                KeyCode::Esc | KeyCode::Char('q') => *menu = None,
                // back towards the chip on the left of the menu's own chip
                KeyCode::Left => {
                    *menu = None;
                    *sig = Sig::Kill;
                }
                _ => {}
            }
            return;
        }
        let cycle = [Sig::Term, Sig::Kill, Sig::Other(*other)];
        let at = cycle.iter().position(|c| c == sig).unwrap_or(0);
        match code {
            KeyCode::Tab | KeyCode::Right => *sig = cycle[(at + 1) % 3],
            KeyCode::BackTab | KeyCode::Left => *sig = cycle[(at + 2) % 3],
            KeyCode::Char('9') => *sig = Sig::Kill,
            KeyCode::Down | KeyCode::Char('o') => *menu = Some(*other),
            KeyCode::Enter if plan.forbidden.is_none() && !plan.is_empty() => {
                let (msg, err) = kill::execute(plan, *sig, self.msg_tx.clone());
                self.status = Some((msg, Instant::now(), err));
                self.modal = Modal::None;
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') => self.modal = Modal::None,
            _ => {}
        }
    }

    fn kill_click(&mut self, col: u16, row: u16) {
        let Some(&(_, _, _, hit)) = self
            .kill_cells
            .iter()
            .find(|(y, a, b, _)| *y == row && col >= *a && col < *b)
        else {
            // a click outside an open menu closes it
            if let Modal::Kill { menu, .. } = &mut self.modal {
                *menu = None;
            }
            return;
        };
        let Modal::Kill { sig, other, menu, .. } = &mut self.modal else {
            return;
        };
        match hit {
            KillHit::Sig(s) => {
                *sig = s;
                *menu = None;
            }
            KillHit::Menu => *menu = if menu.is_some() { None } else { Some(*other) },
            KillHit::Pick(i) => {
                *other = i;
                *sig = Sig::Other(i);
                *menu = None;
                self.last_other = i;
            }
            KillHit::Key(code) => self.kill_key(code),
        }
    }

    fn open_kill(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let mut plan = kill::plan(&row.node);
        plan.what = [kind_label(row.node.kind), row.node.detail.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · ");
        self.modal = Modal::Kill {
            plan,
            sig: Sig::Term,
            other: self.last_other,
            menu: None,
        };
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.filter_mode {
            match k.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_mode = false;
                }
                KeyCode::Enter => self.filter_mode = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => return,
            }
            self.rebuild();
            return;
        }
        match &mut self.modal {
            Modal::Help => {
                self.modal = Modal::None;
                return;
            }
            Modal::Kill { .. } => {
                self.kill_key(k.code);
                return;
            }
            Modal::None => {}
        }
        let page = self.table_height.max(1) as isize;
        let sel = self.selected as isize;
        match k.code {
            KeyCode::Char('q') | KeyCode::F(10) => self.quit = true,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Up => self.move_to(sel - 1),
            KeyCode::Down => self.move_to(sel + 1),
            KeyCode::PageUp => self.move_to(sel - page),
            KeyCode::PageDown => self.move_to(sel + page),
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(isize::MAX / 2),
            KeyCode::Right | KeyCode::Char('+') => self.toggle(self.selected, Some(true)),
            KeyCode::Left | KeyCode::Char('-') => {
                let expanded = self.rows.get(self.selected).is_some_and(|r| r.expanded);
                if expanded {
                    self.toggle(self.selected, Some(false));
                } else if let Some(p) = self.parent_of(self.selected) {
                    self.move_to(p as isize);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle(self.selected, None),
            KeyCode::Char('e') => {
                fn all(n: &Node, out: &mut HashSet<String>) {
                    if !n.children.is_empty() && n.kind != Kind::Proc {
                        out.insert(n.key.clone());
                    }
                    n.children.iter().for_each(|c| all(c, out));
                }
                if let Some(m) = &self.model {
                    m.roots.iter().for_each(|r| all(r, &mut self.expanded));
                }
                self.rebuild();
            }
            KeyCode::Char('E') => {
                self.expanded.clear();
                self.rebuild();
            }
            KeyCode::Char('k') | KeyCode::F(9) => self.open_kill(),
            KeyCode::Char('i') | KeyCode::F(3) => self.cfg.info = !self.cfg.info,
            KeyCode::Char('M') | KeyCode::Char('m') => self.set_sort(Col::Mem),
            KeyCode::Char('D') | KeyCode::Char('d') => self.set_sort(Col::Delta),
            KeyCode::Char('S') | KeyCode::Char('s') => self.set_sort(Col::Swap),
            KeyCode::Char('W') | KeyCode::Char('w') => self.set_sort(Col::Psi),
            KeyCode::Char('P') | KeyCode::Char('p') => self.set_sort(Col::Cpu),
            KeyCode::Char('G') | KeyCode::Char('g') if self.gpu => self.set_sort(Col::Gpu),
            KeyCode::Char('V') | KeyCode::Char('v') if self.gpu => self.set_sort(Col::Vram),
            KeyCode::Char('O') | KeyCode::Char('o') => self.set_sort(Col::Io),
            KeyCode::Char('C') => self.set_sort(Col::Cache),
            KeyCode::Char('N') | KeyCode::Char('n') => self.set_sort(Col::Name),
            KeyCode::Char('<') => self.cycle_sort(-1),
            KeyCode::Char('>') | KeyCode::F(6) => self.cycle_sort(1),
            KeyCode::Char('I') => {
                self.cfg.desc = !self.cfg.desc;
                self.rebuild();
            }
            KeyCode::Char('/') => self.filter_mode = true,
            KeyCode::Char('t') => {
                self.cfg.split = !self.cfg.split;
                let _ = self.cmd_tx.send(Cmd::SetSplit(self.cfg.split));
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.modal = Modal::Help,
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if !matches!(self.modal, Modal::None) {
            match (&self.modal, m.kind) {
                (Modal::Help, MouseEventKind::Down(_)) => self.modal = Modal::None,
                (Modal::Kill { .. }, MouseEventKind::Down(MouseButton::Left)) => self.kill_click(m.column, m.row),
                _ => {}
            }
            return;
        }
        match m.kind {
            MouseEventKind::ScrollDown => {
                let h = self.table_height.max(1) as usize;
                self.offset = (self.offset + 3).min(self.rows.len().saturating_sub(h));
                if self.selected < self.offset {
                    self.move_to(self.offset as isize);
                }
            }
            MouseEventKind::ScrollUp => {
                self.offset = self.offset.saturating_sub(3);
                let h = self.table_height.max(1) as usize;
                if self.selected >= self.offset + h {
                    self.move_to((self.offset + h - 1) as isize);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if m.row == self.row_actions_y
                    && let Some(&(_, _, code)) = self
                        .row_actions
                        .iter()
                        .find(|(a, b, _)| m.column >= *a && m.column < *b)
                {
                    self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
                    return;
                }
                if m.row == self.footer_y {
                    // the first matching segment wins, and single-key segments come first for multi-key chips
                    let hit = self
                        .footer_cells
                        .iter()
                        .find(|(a, b, _)| m.column >= *a && m.column < *b)
                        .map(|c| c.2);
                    let code = match hit {
                        Some(KeyCode::Null) if self.expanded.is_empty() => KeyCode::Char('e'),
                        Some(KeyCode::Null) => KeyCode::Char('E'),
                        Some(c) => c,
                        None => return,
                    };
                    self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
                    return;
                }
                if m.row == self.table_top.saturating_sub(1) {
                    let hit = self
                        .header_cells
                        .iter()
                        .find(|(_, x, w)| m.column >= *x && m.column < x + w);
                    if let Some(col) = hit.and_then(|(c, _, _)| c.sort_col()) {
                        self.set_sort(col);
                    }
                    return;
                }
                if m.row < self.table_top || m.row >= self.table_top + self.table_height {
                    return;
                }
                let i = self.offset + (m.row - self.table_top) as usize;
                if i >= self.rows.len() {
                    return;
                }
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(t, j)| j == i && now.duration_since(t) < Duration::from_millis(400));
                let arrow_x = self.name_x + 2 * self.rows[i].depth as u16;
                self.move_to(i as isize);
                if double || m.column == arrow_x {
                    self.toggle(i, None);
                    self.last_click = None;
                } else {
                    self.last_click = Some((now, i));
                }
            }
            _ => {}
        }
    }

    /// Every optional column is already gone: the width left belongs to the name alone.
    fn minimum_layout(cols: &[ViewCol]) -> bool {
        !cols.iter().any(|c| DROP_ORDER.contains(c))
    }

    fn columns(&self, width: u16) -> Vec<ViewCol> {
        let mut cols = vec![
            ViewCol::Mem,
            ViewCol::Share,
            ViewCol::Delta,
            ViewCol::Swap,
            ViewCol::Psi,
            ViewCol::Cpu,
            ViewCol::Gpu,
            ViewCol::Vram,
            ViewCol::Io,
            ViewCol::Cache,
            ViewCol::Procs,
            ViewCol::Name,
        ];
        if !self.gpu {
            cols.retain(|c| *c != ViewCol::Gpu && *c != ViewCol::Vram);
        }
        // drop the least important columns until the name keeps 30 cells
        for drop in DROP_ORDER {
            let fixed: u16 = cols
                .iter()
                .filter(|c| **c != ViewCol::Name)
                .map(|c| c.width() + GAP)
                .sum();
            if width >= fixed + 30 {
                break;
            }
            cols.retain(|c| *c != drop);
        }
        cols
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        if area.height < 7 {
            return;
        }
        let summary_y = area.y;
        // a lower-eighth-block rule sits on the header band like a border, so the summary
        // meters do not read as column labels
        buf.set_string(
            area.x,
            area.y + 1,
            "▁".repeat(area.width as usize),
            Style::new().fg(TRACK),
        );
        let header_y = area.y + 2;
        let footer_y = area.bottom() - 1;
        let info_h: u16 = if self.cfg.info && area.height >= 20 { 8 } else { 0 };
        self.table_top = header_y + 1;
        self.table_height = footer_y.saturating_sub(self.table_top + info_h);
        self.clamp_selection();

        match &self.model {
            Some(m) => draw_summary(
                &m.summary,
                self.interval,
                Rect::new(area.x, summary_y, area.width, 1),
                buf,
            ),
            None => buf.set_string(area.x + 1, summary_y, tr("measuring…", "mérés…"), Style::new().fg(DIM)),
        }

        // header: data columns first, the name last, so every number sits next to its name
        let cols = self.columns(area.width);
        let show_actions = !Self::minimum_layout(&cols);
        buf.set_style(
            Rect::new(area.x, header_y, area.width, 1),
            Style::new().bg(HEADER_BG).fg(HEADER_FG),
        );
        let mut x = area.x + 1;
        self.header_cells.clear();
        for c in &cols {
            let w = if *c == ViewCol::Name {
                area.right().saturating_sub(x + 1)
            } else {
                c.width()
            };
            let active = c.sort_col() == Some(self.cfg.sort);
            let arrow = if self.cfg.desc { "▼" } else { "▲" };
            let title = if active {
                format!("{arrow}{}", c.title())
            } else {
                c.title().to_string()
            };
            let style = if active {
                Style::new().bg(HEADER_BG).fg(AMBER).add_modifier(Modifier::BOLD)
            } else {
                Style::new().bg(HEADER_BG).fg(HEADER_FG)
            };
            let tw = title.width() as u16;
            let left = matches!(c, ViewCol::Name | ViewCol::Share);
            let tx = if left { x } else { x + w.saturating_sub(tw) };
            buf.set_stringn(tx, header_y, &title, w as usize, style);
            self.header_cells.push((*c, x, w + GAP));
            if *c == ViewCol::Name {
                self.name_x = x;
            }
            x += w + GAP;
        }

        let totals = self.model.as_ref().map(share_totals);
        let mut row_actions = (Vec::new(), u16::MAX);
        for (line, i) in (self.offset..self.rows.len())
            .take(self.table_height as usize)
            .enumerate()
        {
            let y = self.table_top + line as u16;
            let row = &self.rows[i];
            let selected = i == self.selected;
            let base = if selected {
                Style::new().bg(SEL_BG).fg(Color::White)
            } else {
                Style::new()
            };
            if selected {
                buf.set_style(Rect::new(area.x, y, area.width, 1), base);
            }
            let n = &row.node;
            let mut x = area.x + 1;
            for c in &cols {
                match c {
                    ViewCol::Name => {
                        let w = area.right().saturating_sub(x + 1);
                        if selected && show_actions {
                            // the name gives up room for at least the bare k / i chips
                            let reserve = if row.node.targets.is_empty() { 4 } else { 8 };
                            let end = draw_name(row, x, y, w.saturating_sub(reserve), base, buf);
                            row_actions = (draw_row_actions(row, end, x + w, y, buf), y);
                        } else {
                            draw_name(row, x, y, w, base, buf);
                        }
                    }
                    ViewCol::Share => {
                        if let Some(t) = &totals {
                            draw_share(value(n, self.cfg.sort), t.of(self.cfg.sort), x, y, c.width(), base, buf);
                        }
                    }
                    _ => {
                        let (text, style) = cell(n, *c, base);
                        let tw = text.width() as u16;
                        buf.set_stringn(x + c.width().saturating_sub(tw), y, &text, c.width() as usize, style);
                    }
                }
                x += c.width() + GAP;
            }
        }
        (self.row_actions, self.row_actions_y) = row_actions;
        if self.rows.is_empty() && self.model.is_some() {
            let msg = if self.filter.is_empty() {
                tr("no data", "nincs adat")
            } else {
                tr("nothing matches this filter", "nincs találat erre a szűrőre")
            };
            buf.set_string(area.x + 2, self.table_top, msg, Style::new().fg(DIM));
        }

        if info_h > 0 {
            self.draw_info(Rect::new(area.x, footer_y - info_h, area.width, info_h), buf);
        }
        self.draw_footer(Rect::new(area.x, footer_y, area.width, 1), buf);
        match &self.modal {
            Modal::Help => draw_help(area, buf),
            Modal::Kill { plan, sig, menu, .. } => self.kill_cells = draw_kill(plan, *sig, *menu, area, buf),
            Modal::None => {}
        }
    }

    fn draw_footer(&mut self, area: Rect, buf: &mut Buffer) {
        self.footer_cells.clear();
        self.footer_y = area.y;
        let key = Style::new().fg(Color::Black).bg(AMBER).add_modifier(Modifier::BOLD);
        let mut x = area.x;
        if self.filter_mode {
            let label = tr(" Filter: ", " Szűrés: ");
            let (nx, _) = buf.set_stringn(
                x,
                area.y,
                label,
                usize::MAX,
                Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
            );
            x = nx;
            let (nx, _) = buf.set_stringn(x, area.y, &self.filter, usize::MAX, Style::new());
            buf.set_string(nx, area.y, "▏", Style::new().fg(AMBER));
            buf.set_string(
                nx + 2,
                area.y,
                tr("Enter: done   Esc: clear", "Enter: kész   Esc: törlés"),
                Style::new().fg(DIM),
            );
            return;
        }
        if let Some((msg, at, err)) = &self.status
            && at.elapsed() < Duration::from_secs(8)
        {
            let err = *err;
            let style = Style::new()
                .fg(if err { CORAL } else { GOOD })
                .add_modifier(Modifier::BOLD);
            buf.set_stringn(x + 1, area.y, msg, area.width.saturating_sub(2) as usize, style);
            return;
        }
        let view = if self.cfg.split {
            tr("view: per terminal", "nézet: terminálonként")
        } else {
            tr("view: per program", "nézet: programonként")
        };
        let hints: Vec<(&str, String)> = vec![
            ("?", tr("help", "súgó").into()),
            ("k", tr("stop", "leállítás").into()),
            ("i", tr("details", "részletek").into()),
            (
                "/",
                if self.filter.is_empty() {
                    tr("filter", "szűrés").into()
                } else {
                    format!("{}: {}", tr("filter", "szűrő"), self.filter)
                },
            ),
            ("t", view.into()),
            ("< >", tr("sort", "rendezés").into()),
            ("e/E", tr("expand/collapse", "kibont/becsuk").into()),
            ("q", tr("quit", "kilép").into()),
        ];
        for (k, l) in hints {
            if x + (k.width() + l.width() + 4) as u16 > area.right() {
                break;
            }
            let (chip_end, _) = buf.set_stringn(x, area.y, format!(" {k} "), usize::MAX, key);
            let (nx, _) = buf.set_stringn(chip_end + 1, area.y, &l, usize::MAX, Style::new());
            // multi-key chips ("< >", "e/E") map each key character to itself
            let keys: Vec<(u16, char)> = k
                .chars()
                .enumerate()
                .filter(|(_, c)| *c != ' ' && *c != '/' || k == "/")
                .map(|(i, c)| (x + 1 + i as u16, c))
                .collect();
            let default = match k {
                "< >" => KeyCode::Char('>'),
                "e/E" => KeyCode::Null,
                _ => KeyCode::Char(k.chars().next().unwrap_or(' ')),
            };
            if keys.len() > 1 {
                for &(cx, c) in &keys {
                    self.footer_cells.push((cx, cx + 1, KeyCode::Char(c)));
                }
            }
            self.footer_cells.push((x, nx, default));
            x = nx + 2;
        }
    }

    fn refresh_info(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            self.info = None;
            return;
        };
        if self
            .info
            .as_ref()
            .is_some_and(|i| i.key == row.key && i.generation == self.generation)
        {
            return;
        }
        // the process with the most memory stands for the row
        let main = row
            .node
            .pids
            .iter()
            .take(600)
            .filter_map(|&(pid, _)| Some((pid, rss_of(pid)?)))
            .max_by_key(|(_, rss)| *rss)
            .and_then(|(pid, _)| proc_info(pid));
        self.info = Some(Info {
            key: row.key.clone(),
            generation: self.generation,
            main,
        });
    }

    fn draw_info(&mut self, area: Rect, buf: &mut Buffer) {
        self.refresh_info();
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let n = &row.node;
        let bg = Style::new();
        buf.set_style(area, bg);
        let title_style = Style::new().bg(HEADER_BG).fg(HEADER_FG);
        buf.set_style(Rect::new(area.x, area.y, area.width, 1), title_style);
        let (tx, _) = buf.set_stringn(
            area.x + 1,
            area.y,
            display_name(n),
            area.width as usize - 2,
            title_style.add_modifier(Modifier::BOLD),
        );
        buf.set_stringn(
            tx + 2,
            area.y,
            kind_label(n.kind),
            area.width.saturating_sub(tx + 3) as usize,
            title_style.fg(AMBER),
        );

        let w = area.width.saturating_sub(2) as usize;
        let label = Style::new().fg(DIM);
        let mut y = area.y + 1;
        let mut line = |parts: Vec<(String, Style)>, buf: &mut Buffer| {
            let mut x = area.x + 1;
            for (t, s) in parts {
                let (nx, _) = buf.set_stringn(x, y, &t, (area.right() - 1).saturating_sub(x) as usize, s);
                x = nx;
            }
            y += 1;
        };

        let mut stats = vec![
            (tr("Memory ", "Memória ").to_string(), label),
            (fmt_bytes(n.mem), magnitude(n.mem, bg)),
            ("   Swap ".into(), label),
            (fmt_bytes(n.swap), magnitude(n.swap, bg)),
            (tr("   Disk ", "   Lemez ").into(), label),
            (
                format!(
                    "{} {}/s, {} {}/s",
                    tr("read", "olvas"),
                    fmt_bytes(n.io_read as u64),
                    tr("write", "ír"),
                    fmt_bytes(n.io_write as u64)
                ),
                bg,
            ),
            ("   CPU ".into(), label),
            (format!("{:.1}%", n.cpu), bg),
        ];
        if self.gpu {
            stats.push(("   GPU ".into(), label));
            stats.push((format!("{:.0}%, {}", n.gpu, fmt_bytes(n.vram)), bg));
        }
        if let Some(p) = n.psi {
            stats.push((tr("   pressure ", "   nyomás ").into(), label));
            stats.push((format!("{p:.1}%"), bg));
        }
        line(stats, buf);

        let hist = self.history.lock().map(|h| h.recent(&row.key, 60)).unwrap_or_default();
        if hist.len() > 1 {
            let (lo, hi) = (*hist.iter().min().unwrap(), *hist.iter().max().unwrap());
            line(
                vec![
                    (tr("Last 10 min ", "Utolsó 10 perc ").into(), label),
                    (sparkline(&hist), Style::new().fg(AMBER)),
                    (format!("  {} … {}", fmt_bytes(lo), fmt_bytes(hi)), label),
                ],
                buf,
            );
        } else {
            line(
                vec![
                    (tr("Last 10 min ", "Utolsó 10 perc ").into(), label),
                    (
                        tr(
                            "collecting (one point every 10 s)",
                            "gyűjtés folyamatban (10 mp-enként egy pont)",
                        )
                        .into(),
                        label,
                    ),
                ],
                buf,
            );
        }

        let procs = count(n.pids.len(), "process", "processes", "folyamat");
        match self.info.as_ref().and_then(|i| i.main.as_ref()) {
            Some(p) => {
                let who = if n.pids.len() > 1 {
                    tr("largest: ", "legnagyobb: ")
                } else {
                    ""
                };
                line(
                    vec![
                        (procs, bg),
                        (format!("   {who}PID "), label),
                        (p.pid.to_string(), bg),
                        (tr("   parent ", "   szülő ").into(), label),
                        (p.ppid.to_string(), bg),
                        (tr("   running ", "   fut ").into(), label),
                        (fmt_age(p.age), bg),
                        (tr("   user ", "   felhasználó ").into(), label),
                        (p.user.clone(), bg),
                    ],
                    buf,
                );
                line(
                    vec![
                        (tr("Command ", "Parancs ").into(), label),
                        (clip(&p.cmdline, w.saturating_sub(8)), bg),
                    ],
                    buf,
                );
                line(
                    vec![(tr("Directory ", "Könyvtár ").into(), label), (p.cwd.clone(), bg)],
                    buf,
                );
            }
            None => {
                line(vec![(procs, bg)], buf);
                line(Vec::new(), buf);
                line(Vec::new(), buf);
            }
        }
        line(
            vec![(tr("On stop ", "Leállításkor ").into(), label), (target_text(n), bg)],
            buf,
        );
    }
}

fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::App => tr("application", "alkalmazás"),
        Kind::Terminal => tr("terminal", "terminál"),
        Kind::Browser => tr("browser", "böngésző"),
        Kind::Docker => "docker",
        Kind::UserService => tr("user service", "felhasználói szolgáltatás"),
        Kind::SystemService => tr("system service", "rendszerszolgáltatás"),
        Kind::Kernel => "kernel",
        Kind::Job => tr("started from a terminal", "terminálból indítva"),
        Kind::Group => tr("group", "csoport"),
        Kind::Proc => tr("process", "folyamat"),
        Kind::Rest => tr("summary", "összesítés"),
    }
}

fn target_text(n: &Node) -> String {
    if n.targets.is_empty() {
        return tr("cannot be stopped", "nem állítható le").into();
    }
    let mut cg = 0;
    let mut procs = 0;
    let mut docker = Vec::new();
    let mut first_cg = None;
    for t in &n.targets {
        match t {
            Target::Cgroup { path, .. } => {
                cg += 1;
                first_cg.get_or_insert(path.rsplit('/').next().unwrap_or(path).to_string());
            }
            Target::Procs(v) => procs += v.len(),
            Target::Docker { name, .. } => docker.push(name.clone()),
        }
    }
    let mut parts = Vec::new();
    if cg == 1 {
        parts.push(format!(
            "{} ({})",
            tr("the whole cgroup", "a teljes cgroup"),
            first_cg.unwrap_or_default()
        ));
    } else if cg > 1 {
        parts.push(count(cg, "cgroup", "cgroups", "cgroup"));
    }
    if procs > 0 {
        parts.push(count(procs, "process", "processes", "folyamat"));
    }
    if !docker.is_empty() {
        parts.push(format!("docker stop: {}", docker.join(", ")));
    }
    parts.join(", ")
}

fn rss_of(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    s.split_whitespace().nth(1)?.parse().ok()
}

fn proc_info(pid: u32) -> Option<ProcInfo> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest: Vec<&str> = stat[stat.rfind(')')? + 2..].split(' ').collect();
    let ppid = rest.get(1)?.parse().ok()?;
    let start: f64 = rest.get(19)?.parse().ok()?;
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let age = Duration::from_secs_f64((uptime - start / crate::collect::clock_ticks() as f64).max(0.0));
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
        .map(|p| crate::names::tilde(&p))
        .unwrap_or_else(|_| "-".into());
    let uid = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(format!("/proc/{pid}")).ok()?.uid()
    };
    Some(ProcInfo {
        pid,
        ppid,
        user: user_name(uid),
        age,
        cmdline,
        cwd,
    })
}

fn user_name(uid: u32) -> String {
    fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                (f.get(2)?.parse::<u32>().ok()? == uid).then(|| f[0].to_string())
            })
        })
        .unwrap_or_else(|| uid.to_string())
}

fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s} {}", tr("s", "mp")),
        60..3600 => format!("{} {}", s / 60, tr("min", "perc")),
        3600..86400 => format!("{} {} {} {}", s / 3600, tr("h", "ó"), s % 3600 / 60, tr("min", "p")),
        _ => format!("{} {} {} {}", s / 86400, tr("d", "nap"), s % 86400 / 3600, tr("h", "ó")),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.width() + 2 > max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

const SPARK: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

fn sparkline(v: &[u64]) -> String {
    let (lo, hi) = (*v.iter().min().unwrap_or(&0), *v.iter().max().unwrap_or(&0));
    // below 2% of the value the line stays flat instead of magnifying noise
    let span = (hi - lo).max(hi / 50).max(1) as f64;
    v.iter()
        .map(|&x| SPARK[(((x - lo) as f64 / span) * 7.0).round() as usize])
        .collect()
}

struct Totals {
    mem: f64,
    swap: f64,
    vram: f64,
    cpu: f64,
    io: f64,
    procs: f64,
}

impl Totals {
    fn of(&self, col: Col) -> f64 {
        match col {
            Col::Name | Col::Mem | Col::Cache | Col::Delta => self.mem,
            Col::Swap => self.swap,
            Col::Vram => self.vram,
            Col::Cpu => self.cpu,
            Col::Psi | Col::Gpu => 100.0,
            Col::Io => self.io,
            Col::Procs => self.procs,
        }
    }
}

fn share_totals(m: &Model) -> Totals {
    let s = &m.summary;
    Totals {
        mem: s.mem_total as f64,
        swap: s.swap_total as f64,
        vram: s.gpu.as_ref().map(|g| g.total as f64).unwrap_or(0.0),
        cpu: s.ncpu as f64 * 100.0,
        io: m.roots.iter().map(|r| r.io_read + r.io_write).sum(),
        procs: m.roots.iter().map(|r| r.procs).sum::<usize>() as f64,
    }
}

fn magnitude(bytes: u64, base: Style) -> Style {
    const G: u64 = 1 << 30;
    match bytes {
        0 => base.fg(DIM),
        b if b >= 4 * G => base.fg(CORAL).add_modifier(Modifier::BOLD),
        b if b >= G => base.fg(AMBER),
        b if b < 100 << 20 => base.fg(DIM),
        _ => base,
    }
}

fn dot(base: Style) -> (String, Style) {
    ("·".into(), base.fg(DIM))
}

fn cell(n: &Node, c: ViewCol, base: Style) -> (String, Style) {
    match c {
        ViewCol::Mem => (fmt_bytes(n.mem), magnitude(n.mem, base)),
        ViewCol::Swap => (fmt_bytes(n.swap), magnitude(n.swap, base)),
        ViewCol::Delta => match n.delta {
            Some((d, full)) if d.unsigned_abs() >= 1 << 20 => {
                let style = if !full {
                    base.fg(DIM)
                } else if d >= 500 << 20 {
                    base.fg(CORAL).add_modifier(Modifier::BOLD)
                } else if d >= 50 << 20 {
                    base.fg(AMBER)
                } else if d <= -(50 << 20) {
                    base.fg(GOOD)
                } else {
                    base
                };
                (fmt_delta(d), style)
            }
            _ => dot(base),
        },
        ViewCol::Psi => match n.psi {
            Some(p) if p >= 0.05 => {
                let style = if p >= 10.0 {
                    base.fg(CORAL).add_modifier(Modifier::BOLD)
                } else if p >= 1.0 {
                    base.fg(AMBER)
                } else {
                    base
                };
                (format!("{p:.1}"), style)
            }
            _ => dot(base),
        },
        ViewCol::Cache => match n.cache {
            Some(v) if v > 0 => (fmt_bytes(v), base.fg(DIM)),
            _ => dot(base),
        },
        ViewCol::Vram if n.vram > 0 => (fmt_bytes(n.vram), magnitude(n.vram, base)),
        ViewCol::Gpu if n.gpu >= 0.5 => {
            let style = if n.gpu >= 50.0 { base.fg(AMBER) } else { base };
            (format!("{:.0}", n.gpu), style)
        }
        ViewCol::Io => {
            let v = n.io_read + n.io_write;
            if v < 1024.0 {
                dot(base)
            } else {
                let style = if v >= 50.0 * 1048576.0 { base.fg(AMBER) } else { base };
                (fmt_bytes(v as u64), style)
            }
        }
        ViewCol::Cpu => {
            let style = match n.cpu {
                v if v >= 100.0 => base.fg(CORAL).add_modifier(Modifier::BOLD),
                v if v >= 25.0 => base.fg(AMBER),
                v if v < 0.5 => base.fg(DIM),
                _ => base,
            };
            (format!("{:.1}", n.cpu), style)
        }
        ViewCol::Procs => (n.procs.to_string(), base.fg(DIM)),
        _ => dot(base),
    }
}

/// Returns the x just past the last cell written.
fn draw_name(row: &Row, x: u16, y: u16, w: u16, base: Style, buf: &mut Buffer) -> u16 {
    let indent = 2 * row.depth as u16;
    if indent + 2 >= w {
        return x;
    }
    let marker = match (row.expandable, row.expanded) {
        (true, true) => "▾",
        (true, false) => "▸",
        _ => " ",
    };
    buf.set_string(x + indent, y, marker, base.fg(AMBER));
    let n = &row.node;
    let name_style = match n.kind {
        Kind::Rest => base.fg(DIM).add_modifier(Modifier::ITALIC),
        _ if row.depth == 0 => base.add_modifier(Modifier::BOLD),
        _ => base,
    };
    let avail = (w - indent - 2) as usize;
    let name = display_name(n);
    let used = put_clipped(buf, x + indent + 2, y, &name, avail, name_style);
    let mut end = x + indent + 2 + used as u16;
    if !n.detail.is_empty() && used + 3 < avail {
        let d = put_clipped(buf, end + 2, y, &n.detail, avail - used - 2, base.fg(DIM));
        end += 2 + d as u16;
    }
    end
}

/// Action chips at the right end of the selected row; returns their hit segments.
fn draw_row_actions(row: &Row, name_end: u16, right: u16, y: u16, buf: &mut Buffer) -> Vec<(u16, u16, KeyCode)> {
    let key = Style::new().fg(Color::Black).bg(AMBER).add_modifier(Modifier::BOLD);
    let label = Style::new().bg(SEL_BG).fg(Color::White);
    let mut actions = vec![('i', tr("details", "részletek"))];
    if !row.node.targets.is_empty() {
        actions.insert(0, ('k', tr("stop", "leállítás")));
    }
    let chips_w: u16 = actions.len() as u16 * 4;
    let full_w: u16 = actions.iter().map(|(_, l)| 4 + l.width() as u16 + 1).sum();
    let with_labels = name_end + 2 + full_w <= right;
    let mut x = right.saturating_sub(if with_labels { full_w } else { chips_w });
    let mut cells = Vec::new();
    for (k, l) in actions {
        let start = x;
        let (nx, _) = buf.set_stringn(x, y, format!(" {k} "), usize::MAX, key);
        x = nx;
        if with_labels {
            let (nx, _) = buf.set_stringn(x, y, format!(" {l}"), usize::MAX, label);
            x = nx;
        }
        cells.push((start, x, KeyCode::Char(k)));
        x += 1;
    }
    cells
}

/// Writes text, ending in "…" if it does not fit; returns the cells used.
fn put_clipped(buf: &mut Buffer, x: u16, y: u16, text: &str, max: usize, style: Style) -> usize {
    let s = clip(text, max);
    buf.set_stringn(x, y, &s, max, style);
    s.width()
}

const EIGHTHS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

fn bar(frac: f64, cells: usize) -> String {
    let steps = (frac.clamp(0.0, 1.0) * (cells * 8) as f64).round() as usize;
    let mut s = "█".repeat(steps / 8);
    if steps / 8 < cells {
        s.push_str(EIGHTHS[steps % 8]);
        s.push_str(&" ".repeat(cells - steps / 8 - 1));
    }
    s
}

fn draw_share(v: f64, total: f64, x: u16, y: u16, w: u16, base: Style, buf: &mut Buffer) {
    if total <= 0.0 {
        return;
    }
    let frac = (v / total).max(0.0);
    let bar_w = w.saturating_sub(6) as usize;
    let color = if frac >= 0.25 {
        CORAL
    } else if frac >= 0.05 {
        AMBER
    } else {
        GOOD
    };
    buf.set_string(
        x,
        y,
        bar(frac, bar_w),
        base.fg(color).bg(if base.bg.is_some() { SEL_BG } else { TRACK }),
    );
    let pct = if frac >= 0.0995 {
        format!("{:.0}%", frac * 100.0)
    } else {
        format!("{:.1}%", frac * 100.0)
    };
    let pw = pct.width() as u16;
    buf.set_string(x + w - pw, y, pct, if frac < 0.001 { base.fg(DIM) } else { base });
}

fn draw_summary(s: &Summary, interval: Duration, area: Rect, buf: &mut Buffer) {
    let psi = s.mem_psi.unwrap_or(0.0);
    // fixed-width texts so the meters do not shift as the numbers change
    let mut meters: Vec<(&str, f64, String)> = vec![
        ("CPU", s.cpu_pct / 100.0, format!("{:>3.0}%", s.cpu_pct)),
        (
            "RAM",
            s.mem_used as f64 / s.mem_total.max(1) as f64,
            format!(
                "{:>5}/{} {} {:>4.1}%",
                fmt_bytes(s.mem_used),
                fmt_bytes(s.mem_total),
                tr("pressure", "nyomás"),
                psi
            ),
        ),
    ];
    if s.swap_total > 0 {
        meters.push((
            "Swap",
            s.swap_used as f64 / s.swap_total as f64,
            format!(
                "{:>5}/{} {} {:>4}/s {} {:>4}/s",
                fmt_bytes(s.swap_used),
                fmt_bytes(s.swap_total),
                tr("in", "be"),
                fmt_bytes(s.swap_in as u64),
                tr("out", "ki"),
                fmt_bytes(s.swap_out as u64)
            ),
        ));
    }
    if let Some(g) = &s.gpu {
        meters.push((
            "VRAM",
            g.used as f64 / g.total.max(1) as f64,
            format!("{:>5}/{} GPU {:>3}%", fmt_bytes(g.used), fmt_bytes(g.total), g.util),
        ));
    }
    let tail = format!("{:.0} {}", interval.as_secs_f64(), tr("s", "mp"));
    let text_w: usize = meters.iter().map(|(l, _, t)| l.width() + t.width() + 5).sum();
    let bar_w = ((area.width as usize).saturating_sub(text_w + tail.width() + 2) / meters.len()).clamp(4, 20);
    let mut x = area.x + 1;
    for (label, frac, text) in meters {
        let (nx, _) = buf.set_stringn(x, area.y, label, usize::MAX, Style::new().add_modifier(Modifier::BOLD));
        let color = if frac >= 0.9 {
            CORAL
        } else if frac >= 0.7 {
            AMBER
        } else {
            GOOD
        };
        let (nx, _) = buf.set_stringn(
            nx + 1,
            area.y,
            bar(frac, bar_w),
            usize::MAX,
            Style::new().fg(color).bg(TRACK),
        );
        let (nx, _) = buf.set_stringn(nx + 1, area.y, &text, usize::MAX, Style::new());
        x = nx + 3;
        if x >= area.right() {
            return;
        }
    }
    let tw = tail.width() as u16;
    if x + tw < area.right() {
        buf.set_string(area.right() - tw - 1, area.y, tail, Style::new().fg(DIM));
    }
}

fn panel(area: Rect, w: u16, h: u16, buf: &mut Buffer) -> Rect {
    let w = w.min(area.width.saturating_sub(4));
    let h = h.min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    let bg = Style::new().bg(HEADER_BG).fg(HEADER_FG);
    for y in r.y..r.bottom() {
        buf.set_string(r.x, y, " ".repeat(r.width as usize), bg);
    }
    r
}

fn draw_help(area: Rect, buf: &mut Buffer) {
    // (key, text); an empty key with text is a heading, both empty is a spacer
    let hu = crate::i18n::lang() == crate::i18n::Lang::Hu;
    let lines: &[(&str, &str)] = if hu {
        &[
            ("", "Oszlopok"),
            (
                "Memória",
                "amit a kernel nem tud visszavenni (anonim, megosztott, kernel)",
            ),
            (
                "Arány",
                "a rendező oszlop a teljes RAM / swap / VRAM / összes mag arányában",
            ),
            (
                "Δ5p",
                "memóriaváltozás az utolsó 5 percben (halvány: még nincs 5 perc adat)",
            ),
            ("Swap", "lemezre kilapozott memória"),
            (
                "Nyomás",
                "az idő hány %-ában várt a program memóriára (PSI, 10 mp átlag)",
            ),
            ("CPU% / GPU%", "100% = egy teljes CPU mag / a teljes GPU"),
            ("Lemez/s", "olvasás + írás a lemezre (csak saját folyamatok)"),
            ("Cache", "fájl cache, szükség esetén felszabadul"),
            (
                "egyéb",
                "cgroup-szintű maradék; kibontva: zswap, laptáblák, swap cache…",
            ),
            ("", ""),
            ("", "Billentyűk"),
            (
                "↑↓ PgUp PgDn",
                "mozgás;  → ← Enter: kibontás, becsukás;  e / E: mindent",
            ),
            ("M D S W P", "rendezés: memória, változás, swap, nyomás, CPU"),
            (
                "G V O C N",
                "rendezés: GPU%, VRAM, lemez, cache, név;  < >: váltás;  I: irány",
            ),
            ("k  F9", "leállítás (megerősítés után)"),
            ("i  F3", "részletek panel"),
            ("/", "szűrés névre vagy könyvtárra, Esc törli"),
            ("t", "terminálban indított programok: saját sor / a terminál alatt"),
            ("q", "kilépés (a rendezés és a nézet megmarad)"),
            ("", ""),
            (
                "egér",
                "fejléc: rendez; sor: dupla kattintás kibont; a k / i gombok és a lábsor is kattintható",
            ),
        ]
    } else {
        &[
            ("", "Columns"),
            ("Memory", "what the kernel cannot reclaim (anonymous, shared, kernel)"),
            (
                "Share",
                "the sort column as a share of all RAM / swap / VRAM / all cores",
            ),
            (
                "Δ5m",
                "memory change over the last 5 minutes (dim: less than 5 minutes of data)",
            ),
            ("Swap", "memory paged out to disk"),
            (
                "Pressure",
                "% of time the program waited for memory (PSI, 10 s average)",
            ),
            ("CPU% / GPU%", "100% = one whole CPU core / the whole GPU"),
            ("Disk/s", "disk reads + writes (own processes only)"),
            ("Cache", "file cache, freed when memory is needed"),
            (
                "other",
                "cgroup-level remainder; expand for zswap, page tables, swap cache…",
            ),
            ("", ""),
            ("", "Keys"),
            ("↑↓ PgUp PgDn", "move;  → ← Enter: expand, collapse;  e / E: everything"),
            ("M D S W P", "sort: memory, change, swap, pressure, CPU"),
            (
                "G V O C N",
                "sort: GPU%, VRAM, disk, cache, name;  < >: cycle;  I: invert",
            ),
            ("k  F9", "stop (after confirmation)"),
            ("i  F3", "details panel"),
            ("/", "filter by name or directory, Esc clears"),
            ("t", "programs started in terminals: own rows / under the terminal"),
            ("q", "quit (sort and view are remembered)"),
            ("", ""),
            (
                "mouse",
                "header: sort; row: double-click expands; the k / i chips and the footer are clickable",
            ),
        ]
    };
    let r = panel(area, 92, lines.len() as u16 + 2, buf);
    let bg = Style::new().bg(HEADER_BG).fg(HEADER_FG);
    for (i, (k, v)) in lines.iter().enumerate().take(r.height.saturating_sub(2) as usize) {
        let y = r.y + 1 + i as u16;
        if k.is_empty() {
            buf.set_stringn(
                r.x + 2,
                y,
                v,
                (r.width - 4) as usize,
                bg.fg(AMBER).add_modifier(Modifier::BOLD),
            );
        } else {
            buf.set_stringn(r.x + 2, y, k, 14, bg.fg(AMBER));
            buf.set_stringn(r.x + 17, y, v, (r.width - 19) as usize, bg);
        }
    }
}

fn draw_kill(
    plan: &Plan,
    sig: Sig,
    menu: Option<usize>,
    area: Rect,
    buf: &mut Buffer,
) -> Vec<(u16, u16, u16, KillHit)> {
    let bg = Style::new().bg(HEADER_BG).fg(HEADER_FG);
    let warn = bg.fg(CORAL).add_modifier(Modifier::BOLD);
    let dim = bg.fg(DIM);
    let mut lines: Vec<(String, Style)> = Vec::new();
    lines.push((
        format!("{}: {}", tr("Stop", "Leállítás"), plan.title),
        bg.fg(AMBER).add_modifier(Modifier::BOLD),
    ));
    if !plan.what.is_empty() {
        lines.push((plan.what.clone(), dim));
    }
    lines.push((String::new(), bg));
    if let Some(reason) = &plan.forbidden {
        lines.push((
            format!("{}: {reason}.", tr("Cannot be stopped", "Nem állítható le")),
            warn,
        ));
        lines.push((
            tr(
                "apptop does not call sudo; stop it from a root shell.",
                "Az apptop nem hív sudo-t; ehhez root terminálban kell leállítani.",
            )
            .into(),
            dim,
        ));
    } else if plan.is_empty() {
        lines.push((
            tr("No running process on this row.", "Nincs futó folyamat ezen a soron.").into(),
            dim,
        ));
    } else {
        if plan.programs > 1 || plan.pids.len() > 30 {
            let mut parts = Vec::new();
            let programs = plan.programs - plan.containers.len();
            if programs > 1 {
                parts.push(count(programs, "program", "programs", "program"));
            }
            if !plan.pids.is_empty() {
                parts.push(count(plan.pids.len(), "process", "processes", "folyamat"));
            }
            if !plan.containers.is_empty() {
                parts.push(count(
                    plan.containers.len(),
                    "docker container",
                    "docker containers",
                    "docker konténer",
                ));
            }
            let t = match crate::i18n::lang() {
                crate::i18n::Lang::En => format!("Warning: {} will stop!", parts.join(", ")),
                crate::i18n::Lang::Hu => format!("Figyelem: {} áll le!", parts.join(", ")),
            };
            lines.push((t, warn));
        }
        if plan.includes_self {
            lines.push((
                tr(
                    "apptop runs inside this too and will exit with it.",
                    "Az apptop is ebben fut, vele együtt kilép.",
                )
                .into(),
                warn,
            ));
        }
        if !plan.cgroups.is_empty() {
            let names: Vec<&str> = plan.cgroups.iter().map(|c| c.rsplit('/').next().unwrap_or(c)).collect();
            lines.push((format!("cgroup: {}", clip(&names.join(", "), 80)), bg));
        }
        if !plan.containers.is_empty() {
            let verb = if sig == Sig::Term { "docker stop" } else { "docker kill" };
            let names: Vec<&str> = plan.containers.iter().map(|(_, n)| n.as_str()).collect();
            lines.push((format!("{verb}: {}", clip(&names.join(", "), 80)), bg));
        }
        lines.push((String::new(), bg));
        let shown = 8;
        for &(pid, _) in plan.pids.iter().take(shown) {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            let args = fs::read(format!("/proc/{pid}/cmdline"))
                .map(|b| {
                    b.split(|&c| c == 0)
                        .skip(1)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            lines.push((clip(&format!("{pid:>8}  {}  {args}", comm.trim()), 86), dim));
        }
        if plan.pids.len() > shown {
            lines.push((
                format!(
                    "          … {} {}",
                    tr("and", "és még"),
                    count(plan.pids.len() - shown, "more process", "more processes", "folyamat")
                ),
                dim,
            ));
        }
    }
    lines.push((String::new(), bg));

    let r = panel(area, 92, lines.len() as u16 + 7, buf);
    for (i, (t, s)) in lines.iter().enumerate() {
        buf.set_stringn(r.x + 2, r.y + 1 + i as u16, t, (r.width - 4) as usize, *s);
    }
    let mut cells = Vec::new();
    let key = Style::new().fg(Color::Black).bg(AMBER).add_modifier(Modifier::BOLD);
    let actions_y = r.bottom() - 2;
    let chip_row = |y: u16, items: &[(&str, &str, KeyCode)], buf: &mut Buffer, cells: &mut Vec<_>| {
        let mut x = r.x + 2;
        for (k, l, code) in items {
            let start = x;
            let (nx, _) = buf.set_stringn(x, y, format!(" {k} "), usize::MAX, key);
            let (nx, _) = buf.set_stringn(nx + 1, y, *l, usize::MAX, bg);
            cells.push((y, start, nx, KillHit::Key(*code)));
            x = nx + 3;
        }
    };
    if plan.forbidden.is_some() || plan.is_empty() {
        chip_row(
            actions_y,
            &[("Esc", tr("back", "vissza"), KeyCode::Esc)],
            buf,
            &mut cells,
        );
        return cells;
    }

    // signal chips: the chosen one amber, the others as quiet buttons
    let sig_y = r.bottom() - 5;
    let on = Style::new().fg(Color::Black).bg(AMBER).add_modifier(Modifier::BOLD);
    let off = Style::new().fg(HEADER_FG).bg(SEL_BG);
    let (mut x, _) = buf.set_stringn(r.x + 2, sig_y, tr("Signal: ", "Jel: "), usize::MAX, bg);
    let other_label = match sig {
        Sig::Other(i) => format!(" {} ▾ ", OTHER[i].1),
        _ => format!(" {} ▾ ", tr("other…", "egyéb…")),
    };
    let menu_x = {
        let mut menu_x = x;
        for (label, hit, active) in [
            (" SIGTERM ".to_string(), KillHit::Sig(Sig::Term), sig == Sig::Term),
            (" SIGKILL ".to_string(), KillHit::Sig(Sig::Kill), sig == Sig::Kill),
            (other_label, KillHit::Menu, matches!(sig, Sig::Other(_))),
        ] {
            if matches!(hit, KillHit::Menu) {
                menu_x = x;
            }
            let (nx, _) = buf.set_stringn(x, sig_y, &label, usize::MAX, if active { on } else { off });
            cells.push((sig_y, x, nx, hit));
            x = nx + 1;
        }
        menu_x
    };
    buf.set_stringn(r.x + 2, sig_y + 1, sig.explain(), (r.width - 4) as usize, dim);
    chip_row(
        actions_y,
        &[
            ("Enter", tr("send", "küldés"), KeyCode::Enter),
            ("Tab", tr("signal", "jel"), KeyCode::Tab),
            ("↓", tr("other signals", "egyéb jelek"), KeyCode::Down),
            ("Esc", tr("cancel", "mégse"), KeyCode::Esc),
        ],
        buf,
        &mut cells,
    );

    if let Some(cur) = menu {
        // drop-down under the "other" chip; its clicks are checked before the rows beneath it
        let w: u16 = 86;
        let x0 = menu_x.min(area.right().saturating_sub(w + 1));
        let y0 = sig_y + 1;
        let h = OTHER.len() as u16 + 2;
        let y0 = if y0 + h > area.bottom() {
            sig_y.saturating_sub(h)
        } else {
            y0
        };
        let box_style = Style::new().bg(SEL_BG).fg(HEADER_FG);
        for yy in y0..y0 + h {
            buf.set_string(x0, yy, " ".repeat(w as usize), box_style);
        }
        let mut menu_cells = Vec::new();
        for (i, (_, name, en, hu)) in OTHER.iter().enumerate() {
            let yy = y0 + 1 + i as u16;
            let style = if i == cur { on } else { box_style };
            if i == cur {
                buf.set_string(x0 + 1, yy, " ".repeat(w as usize - 2), style);
            }
            buf.set_stringn(x0 + 2, yy, *name, 9, style.add_modifier(Modifier::BOLD));
            buf.set_stringn(x0 + 12, yy, tr(en, hu), w as usize - 14, style);
            menu_cells.push((yy, x0, x0 + w, KillHit::Pick(i)));
        }
        menu_cells.extend(cells);
        return menu_cells;
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(fmt_bytes(0), "0");
        assert_eq!(fmt_bytes(512 * 1024), "512K");
        assert_eq!(fmt_bytes(300 << 20), "300M");
        assert_eq!(fmt_bytes(3 << 29), "1.5G");
        assert_eq!(fmt_delta(-(5 << 20)), "-5M");
    }

    #[test]
    fn bars_fill_to_width() {
        assert_eq!(bar(0.0, 4), "    ");
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.5, 4), "██  ");
        assert_eq!(bar(2.0, 3).chars().count(), 3);
    }

    #[test]
    fn config_names_round_trip() {
        for c in Col::ALL {
            assert_eq!(Col::from_name(c.name()), Some(c));
        }
    }
}
