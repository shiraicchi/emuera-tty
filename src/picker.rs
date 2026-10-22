//! The game launcher and first-run picker: a small full-screen list with
//! three pages. The **library** lists the games played or added (eraTW,
//! eraMegaten, eraPoPs…) to launch one; **Add a game** opens a directory
//! browser for an Emuera game folder (one with `CSV/` and `ERB/` inside)
//! or a downloaded archive of one (extracted on choice, see
//! `crate::archive`); **Settings** edits `crate::settings`.
//!
//! Library keys: Enter plays, `a` adds, `d` / Delete removes from the list
//! (files stay), `s` settings, q quits. Settings: ← / → change, Esc back.
//! Browser keys:
//! Keys: ↑/↓ (or k/j), PgUp/PgDn, Home/End move; Enter plays a marked game
//! or opens a folder; → opens a folder; ← / Backspace goes up; `~` jumps
//! home; `.` shows hidden folders; q / Esc cancels. Mouse: click selects,
//! clicking the selected entry again activates it, the wheel scrolls. Works
//! with the same mouse sources as the game screen (SGR / urxvt / X10
//! reports, or `/dev/input/mice` on the Linux console).

use crate::loader::{find_dir_ci, find_file_ci, read_text};
use std::io::Write;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;

/// Is `dir` an Emuera game (has CSV/ and ERB/, any letter case)?
pub fn is_game_dir(dir: &Path) -> bool {
    find_dir_ci(dir, "CSV").is_some() && find_dir_ci(dir, "ERB").is_some()
}

/// The タイトル line of a game's GAMEBASE.csv, if any.
fn game_title(dir: &Path) -> Option<String> {
    let csv = find_dir_ci(dir, "CSV")?;
    let text = read_text(&find_file_ci(&csv, "GAMEBASE.CSV")?).ok()?;
    text.lines().find_map(|l| l.strip_prefix("タイトル,").map(|t| t.trim().to_string())).filter(|t| !t.is_empty())
}

/// One row of the listing.
struct Entry {
    label: String,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Library,
    Browse,
    Settings,
}

#[derive(Clone, PartialEq)]
enum Kind {
    /// library: a game to launch (`ok`: the folder is still there)
    Game {
        path: PathBuf,
        ok: bool,
    },
    AddGame,
    OpenSettings,
    /// settings: `crate::settings::ITEMS[i]`
    Setting(usize),
    Back,
    /// play the current folder (it is a game)
    PlayHere,
    Up,
    Dir {
        path: PathBuf,
        game: bool,
    },
    /// a .zip / .tar.gz / … that may hold a game
    Archive {
        path: PathBuf,
    },
}

/// What a key or click asks for.
enum Act {
    Move(isize),
    Page(isize),
    Top,
    Bottom,
    Activate,
    Open,
    Up,
    Home,
    ToggleHidden,
    Cancel,
    Click(usize),
    /// a letter with no meaning in the browser (library / settings keys)
    Letter(char),
    Delete,
    None,
}

struct Picker {
    dir: PathBuf,
    entries: Vec<Entry>,
    sel: usize,
    top: usize,
    hidden: bool,
    note: String,
    mice: Option<crate::mice::Mice>,
    pending: Vec<u8>,
    mode: Mode,
    /// the browser was opened from the library to add a game
    adding: bool,
    st: crate::settings::Settings,
}

/// The launcher: the game library, with adding games and settings. Returns
/// the game to play (already remembered as the last one), or None.
pub fn launcher() -> Option<PathBuf> {
    let st = crate::settings::Settings::load();
    let start = browse_start(&st);
    run_screen(Mode::Library, &start)
}

/// Where the browser opens: next to the last game, else home.
fn browse_start(st: &crate::settings::Settings) -> PathBuf {
    st.last.as_ref().and_then(|g| g.parent().map(|p| p.to_path_buf())).or_else(crate::sys::home_dir).unwrap_or_else(|| PathBuf::from("."))
}

fn run_screen(mode: Mode, start: &Path) -> Option<PathBuf> {
    let raw_mode = crate::sys::RawMode::enter().ok()?;
    let term = std::env::var("TERM").unwrap_or_default();
    let is_vt = term == "linux" || term.starts_with("fbterm");
    let mice = if crate::mice::Mice::wanted(is_vt) { crate::mice::Mice::open().ok() } else { None };
    out(b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1015h\x1b[?1006h");
    let mut p = Picker {
        dir: start.to_path_buf(),
        entries: Vec::new(),
        sel: 0,
        top: 0,
        hidden: false,
        note: String::new(),
        mice,
        pending: Vec::new(),
        mode,
        adding: false,
        st: crate::settings::Settings::load(),
    };
    p.load();
    let chosen = p.run();
    out(b"\x1b[?1006l\x1b[?1015l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l");
    raw_mode.restore();
    chosen
}

fn out(b: &[u8]) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(b);
    let _ = o.flush();
}

fn term_size() -> (usize, usize) {
    crate::sys::winsize().map_or((80, 24), |(c, r, _, _)| (c, r))
}

/// `s` cut to `w` display cells.
fn fit(s: &str, w: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += UnicodeWidthChar::width(*c).unwrap_or(0);
            used <= w
        })
        .collect()
}

impl Picker {
    /// List the current page.
    fn load(&mut self) {
        self.entries.clear();
        match self.mode {
            Mode::Library => {
                for g in self.st.library.clone() {
                    let ok = is_game_dir(&g);
                    let title = if ok { game_title(&g).unwrap_or_else(|| "Emuera game".into()) } else { "(folder missing)".into() };
                    let last = if self.st.last.as_ref() == Some(&g) { "  · last played" } else { "" };
                    let git = if ok && git_root(&g).is_some() { "  · git" } else { "" };
                    self.entries.push(Entry { label: format!("★ {}   {}{}{}", title, g.display(), last, git), kind: Kind::Game { path: g, ok } });
                }
                self.entries.push(Entry { label: "＋ Add a game (folder or archive)…".into(), kind: Kind::AddGame });
                self.entries.push(Entry { label: "⚙ Settings".into(), kind: Kind::OpenSettings });
                self.sel = self.entries.iter().position(|e| matches!(&e.kind, Kind::Game { path, .. } if self.st.last.as_ref() == Some(path))).unwrap_or(0);
                self.top = 0;
                return;
            }
            Mode::Settings => {
                for (i, it) in crate::settings::ITEMS.iter().enumerate().filter(|(_, it)| self.st.shown(it)) {
                    let v = self.st.get(it.key);
                    let restart = if it.live { "" } else { "  (next start)" };
                    self.entries.push(Entry { label: format!("{:<18} ‹ {} ›{}", it.label, v, restart), kind: Kind::Setting(i) });
                }
                self.entries.push(Entry { label: "← Back".into(), kind: Kind::Back });
                self.sel = self.sel.min(self.entries.len() - 1);
                return;
            }
            Mode::Browse => {}
        }
        if is_game_dir(&self.dir) {
            let t = game_title(&self.dir).unwrap_or_else(|| "this game".into());
            self.entries.push(Entry { label: format!("▶ Play {}", t), kind: Kind::PlayHere });
        }
        if self.dir.parent().is_some() {
            self.entries.push(Entry { label: "../".into(), kind: Kind::Up });
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default();
        dirs.retain(|d| self.hidden || !d.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')));
        dirs.sort_by_key(|d| d.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        for d in dirs {
            let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let game = is_game_dir(&d);
            let label = if game { format!("★ {}/   ({})", name, game_title(&d).unwrap_or_else(|| "Emuera game".into())) } else { format!("  {}/", name) };
            self.entries.push(Entry { label, kind: Kind::Dir { path: d, game } });
        }
        let mut archives: Vec<PathBuf> =
            std::fs::read_dir(&self.dir).map(|rd| rd.flatten().map(|e| e.path()).filter(|p| crate::archive::is_archive(p)).collect()).unwrap_or_default();
        archives.retain(|a| self.hidden || !a.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')));
        archives.sort_by_key(|a| a.file_name().map(|n| n.to_string_lossy().to_lowercase()));
        for a in archives {
            let name = a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let bytes = std::fs::metadata(&a).map(|m| m.len()).unwrap_or(0);
            let size = if bytes >= 1 << 20 { format!("{:.0} MB", bytes as f64 / 1048576.0) } else { format!("{} KB", bytes.div_ceil(1024)) };
            self.entries.push(Entry { label: format!("▣ {}   [archive, {}]", name, size), kind: Kind::Archive { path: a } });
        }
        // start on "Play" or the first folder rather than on ../
        self.sel = self.entries.iter().position(|e| e.kind != Kind::Up).unwrap_or(0);
        self.top = 0;
    }

    fn cd(&mut self, to: PathBuf) {
        let from = self.dir.clone();
        self.dir = to;
        self.load();
        // coming back up: keep the folder we came from selected
        if let Some(i) = self.entries.iter().position(|e| matches!(&e.kind, Kind::Dir { path, .. } if *path == from)) {
            self.sel = i;
        }
    }

    /// Rows available for the list (header 2 lines, footer 2).
    fn list_rows(&self) -> usize {
        term_size().1.saturating_sub(4).max(1)
    }

    fn draw(&mut self) {
        let (cols, _) = term_size();
        let rows = self.list_rows();
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + rows {
            self.top = self.sel + 1 - rows;
        }
        let mut s = String::from("\x1b[H\x1b[0m\x1b[2J");
        match self.mode {
            Mode::Library => {
                s += &format!(
                    "\x1b[1memuera-tty — your games\x1b[0m\r\n\x1b[2m{}\x1b[0m\r\n",
                    fit("Enter to play · a add a game · u update (git) · U update all · d remove from the list · s settings", cols)
                )
            }
            Mode::Settings => {
                s += &format!(
                    "\x1b[1mSettings\x1b[0m\r\n\x1b[2m{}\x1b[0m\r\n",
                    fit(&crate::settings::config_path().map(|p| p.display().to_string()).unwrap_or_default(), cols)
                )
            }
            Mode::Browse => {
                s += &format!(
                    "\x1b[1mChoose an Emuera game folder or archive\x1b[0m  (★ = game, ▣ = archive)\r\n\x1b[2m{}\x1b[0m\r\n",
                    fit(&self.dir.display().to_string(), cols)
                )
            }
        }
        for (i, e) in self.entries.iter().enumerate().skip(self.top).take(rows) {
            let line = fit(&e.label, cols.saturating_sub(1));
            let pad = cols.saturating_sub(line.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>() + 1);
            let color = match e.kind {
                Kind::Game { ok: false, .. } => "\x1b[2m",
                Kind::PlayHere | Kind::Dir { game: true, .. } | Kind::Game { .. } => "\x1b[33m",
                Kind::AddGame | Kind::OpenSettings => "\x1b[36m",
                Kind::Archive { .. } => "\x1b[36m",
                _ => "",
            };
            // the "· last played" (green) and "· git" (sky blue) tags of a game
            // are coloured on their own; the rest of the row keeps its colour
            let back = format!("\x1b[0m{}{}", color, if i == self.sel { "\x1b[7m" } else { "" });
            let line = if matches!(e.kind, Kind::Game { .. }) {
                let tint = |l: String, tag: &str, code: &str| match l.rfind(tag) {
                    Some(at) => format!("{}{}{}{}{}", &l[..at], code, tag, back, &l[at + tag.len()..]),
                    None => l,
                };
                // the tags come after the path, so the last match is theirs
                tint(tint(line, "· last played", "\x1b[32m"), "· git", "\x1b[38;5;117m")
            } else {
                line
            };
            if i == self.sel {
                s += &format!("{}\x1b[7m {}{}\x1b[0m\r\n", color, line, " ".repeat(pad));
            } else {
                s += &format!("{} {}\x1b[0m\r\n", color, line);
            }
        }
        if self.entries.is_empty() {
            s += "  (empty)\r\n";
        }
        let help = match self.mode {
            Mode::Library => "↑↓ move  Enter play  a add  u update  U update all  d remove  s settings  q quit",
            Mode::Settings => "↑↓ move  ←/→ or Enter change  Esc back",
            Mode::Browse if self.adding => "↑↓ move  Enter add/open  → open  ← up  ~ home  . hidden  Esc back",
            Mode::Browse => "↑↓ move  Enter play/open  → open  ← up  ~ home  . hidden  q quit",
        };
        // the selected setting's explanation
        if let (Mode::Settings, Some(Entry { kind: Kind::Setting(i), .. })) = (self.mode, self.entries.get(self.sel)) {
            if self.note.is_empty() {
                self.note = crate::settings::ITEMS[*i].help.to_string();
            }
        }
        let (_, trows) = term_size();
        s += &format!("\x1b[{};1H\x1b[0m{}\x1b[{};1H\x1b[2m{}\x1b[0m", trows.saturating_sub(1), fit(&self.note, cols), trows, fit(help, cols));
        out(s.as_bytes());
    }

    fn run(&mut self) -> Option<PathBuf> {
        loop {
            self.draw();
            let act = self.next_act();
            // (a timeout only redraws: keep the message)
            if !matches!(act, Act::None) {
                self.note.clear();
            }
            let n = self.entries.len();
            let act = match self.mode_act(act) {
                Ok(a) => a,
                Err(r) => {
                    if r.is_some() {
                        return r;
                    }
                    continue;
                }
            };
            match act {
                Act::Move(d) => self.sel = (self.sel as isize + d).clamp(0, n.saturating_sub(1) as isize) as usize,
                Act::Page(d) => {
                    let r = self.list_rows() as isize;
                    self.sel = (self.sel as isize + d * r).clamp(0, n.saturating_sub(1) as isize) as usize;
                }
                Act::Top => self.sel = 0,
                Act::Bottom => self.sel = n.saturating_sub(1),
                Act::Click(i) if i < n => {
                    if i == self.sel {
                        if let Some(p) = self.activate() {
                            return Some(p);
                        }
                    } else {
                        self.sel = i;
                    }
                }
                Act::Activate => {
                    if let Some(p) = self.activate() {
                        return Some(p);
                    }
                }
                Act::Open if matches!(self.entries.get(self.sel), Some(Entry { kind: Kind::Archive { .. }, .. })) => {
                    if let Some(p) = self.activate() {
                        return Some(p);
                    }
                }
                Act::Open => {
                    if let Some(Entry { kind: Kind::Dir { path, .. }, .. }) = self.entries.get(self.sel) {
                        let p = path.clone();
                        self.cd(p);
                    } else if self.entries.get(self.sel).is_some_and(|e| e.kind == Kind::Up) {
                        self.go_up();
                    }
                }
                Act::Up => self.go_up(),
                Act::Home => {
                    if let Some(h) = crate::sys::home_dir() {
                        self.cd(h);
                    }
                }
                Act::ToggleHidden => {
                    self.hidden = !self.hidden;
                    self.load();
                }
                Act::Cancel if self.mode == Mode::Browse && self.adding => {
                    self.adding = false;
                    self.mode = Mode::Library;
                    self.load();
                }
                Act::Cancel if self.mode == Mode::Settings => {
                    self.mode = Mode::Library;
                    self.load();
                }
                Act::Cancel => return None,
                _ => {}
            }
        }
    }

    /// Library and settings keys. Ok(act): handle `act` the usual way;
    /// Err(Some(game)): play it; Err(None): handled.
    fn mode_act(&mut self, act: Act) -> Result<Act, Option<PathBuf>> {
        match (self.mode, &act) {
            (Mode::Library, Act::Letter('a')) => {
                self.start_adding();
                Err(None)
            }
            (Mode::Library, Act::Letter('s')) => {
                self.mode = Mode::Settings;
                self.sel = 0;
                self.load();
                Err(None)
            }
            (Mode::Library, Act::Letter('d') | Act::Delete) => {
                if let Some(Entry { kind: Kind::Game { path, .. }, .. }) = self.entries.get(self.sel) {
                    let p = path.clone();
                    self.st.remove_game(&p);
                    self.st.save();
                    let sel = self.sel;
                    self.load();
                    self.sel = sel.min(self.entries.len().saturating_sub(1));
                    self.note = format!("Removed {} from the list (its files are still there).", p.display());
                }
                Err(None)
            }
            (Mode::Library, Act::Letter('u')) => {
                if let Some(Entry { kind: Kind::Game { path, .. }, .. }) = self.entries.get(self.sel) {
                    let p = path.clone();
                    self.note = match git_root(&p) {
                        None => "This game isn't a git checkout: nothing to update from.".into(),
                        Some(root) => {
                            let (cols, rows) = term_size();
                            out(format!("\x1b[{};1H\x1b[0m\x1b[2K{}", rows.saturating_sub(1), fit(&format!("Updating {} (git pull)…", root.display()), cols))
                                .as_bytes());
                            match git_pull(&root) {
                                Ok(m) => m,
                                Err(e) => format!("\x1b[31m{}\x1b[0m", e),
                            }
                        }
                    };
                    let sel = self.sel;
                    self.load();
                    self.sel = sel.min(self.entries.len().saturating_sub(1));
                }
                Err(None)
            }
            // every game that is a git checkout (each checkout once)
            (Mode::Library, Act::Letter('U')) => {
                let mut roots: Vec<PathBuf> = Vec::new();
                for e in &self.entries {
                    if let Kind::Game { path, ok: true } = &e.kind {
                        if let Some(r) = git_root(path).filter(|r| !roots.contains(r)) {
                            roots.push(r);
                        }
                    }
                }
                if roots.is_empty() {
                    self.note = "None of these games is a git checkout: nothing to update from.".into();
                    return Err(None);
                }
                let (cols, rows) = term_size();
                let (mut updated, mut current, mut failed) = (Vec::new(), 0, Vec::new());
                for (i, root) in roots.iter().enumerate() {
                    out(format!(
                        "\x1b[{};1H\x1b[0m\x1b[2K{}",
                        rows.saturating_sub(1),
                        fit(&format!("Updating {}/{}: {} (git pull)…", i + 1, roots.len(), root.display()), cols)
                    )
                    .as_bytes());
                    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
                    match git_pull(root) {
                        Ok(m) if m.starts_with("Already") => current += 1,
                        Ok(m) => updated.push(format!("{} ({})", name, m.trim_start_matches("Updated: ").trim_end_matches('.'))),
                        Err(e) => failed.push(format!("{}: {}", name, e.trim_start_matches("Update failed: "))),
                    }
                }
                let mut parts = Vec::new();
                if !updated.is_empty() {
                    parts.push(format!("updated {}", updated.join(", ")));
                }
                if current > 0 {
                    parts.push(format!("{} already up to date", current));
                }
                if !failed.is_empty() {
                    parts.push(format!("\x1b[31mfailed {}\x1b[0m", failed.join("; ")));
                }
                self.note = format!("Update all: {}.", parts.join("; "));
                let sel = self.sel;
                self.load();
                self.sel = sel.min(self.entries.len().saturating_sub(1));
                Err(None)
            }
            // ← / Backspace mean nothing on the library
            (Mode::Library, Act::Up) => Err(None),
            (Mode::Settings, Act::Open | Act::Up) => {
                if let Some(Entry { kind: Kind::Setting(i), .. }) = self.entries.get(self.sel) {
                    let key = crate::settings::ITEMS[*i].key;
                    self.st.cycle(key, if matches!(act, Act::Up) { -1 } else { 1 });
                    self.st.save();
                    self.load();
                    return Err(None);
                }
                if matches!(act, Act::Up) {
                    return Ok(Act::Cancel);
                }
                Err(None)
            }
            _ => Ok(act),
        }
    }

    fn start_adding(&mut self) {
        self.adding = true;
        self.mode = Mode::Browse;
        self.dir = browse_start(&self.st);
        self.load();
    }

    /// A game folder was chosen in the browser: add it to the library (when
    /// adding), or play it.
    fn chosen(&mut self, game: PathBuf) -> Option<PathBuf> {
        if !self.adding {
            return Some(game);
        }
        let g = self.st.add_game(&game);
        self.st.save();
        self.adding = false;
        self.mode = Mode::Library;
        self.load();
        self.sel = self.entries.iter().position(|e| matches!(&e.kind, Kind::Game { path, .. } if *path == g)).unwrap_or(0);
        self.note = format!("Added {} — Enter to play.", game_title(&g).unwrap_or_else(|| g.display().to_string()));
        None
    }

    fn go_up(&mut self) {
        if let Some(p) = self.dir.parent().map(Path::to_path_buf) {
            self.cd(p);
        }
    }

    /// Enter on the selected entry: play a game, otherwise open the folder.
    fn activate(&mut self) -> Option<PathBuf> {
        let kind = self.entries.get(self.sel)?.kind.clone();
        match kind {
            Kind::Game { path, ok: true } => return Some(path),
            Kind::Game { path, ok: false } => self.note = format!("\x1b[31m{} is gone (d removes it from the list)\x1b[0m", path.display()),
            Kind::AddGame => self.start_adding(),
            Kind::OpenSettings => {
                self.mode = Mode::Settings;
                self.sel = 0;
                self.load();
            }
            Kind::Setting(i) => {
                self.st.cycle(crate::settings::ITEMS[i].key, 1);
                self.st.save();
                self.load();
            }
            Kind::Back => {
                self.mode = Mode::Library;
                self.load();
            }
            Kind::PlayHere => {
                let d = self.dir.clone();
                return self.chosen(d);
            }
            Kind::Up => self.go_up(),
            Kind::Dir { path, game: true } => return self.chosen(path),
            Kind::Archive { path } => match self.extract(&path) {
                Ok(g) => return self.chosen(g),
                Err(e) => self.note = format!("\x1b[31m{}\x1b[0m", e),
            },
            Kind::Dir { path, game: false } => {
                self.cd(path);
                if self.entries.iter().all(|e| e.kind == Kind::Up) {
                    self.note = "Nothing here: no folders or archives.".into();
                }
            }
        }
        None
    }

    /// Extract an archive, showing progress on the status line.
    fn extract(&mut self, archive: &Path) -> Result<PathBuf, String> {
        let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (cols, rows) = term_size();
        let line = rows.saturating_sub(1);
        out(format!("\x1b[{};1H\x1b[0m\x1b[2K{}", line, fit(&format!("Extracting {}…", name), cols)).as_bytes());
        let mut show = |n: usize| out(format!("\x1b[{};1H\x1b[2K{}", line, fit(&format!("Extracting {}… {} files", name, n), cols)).as_bytes());
        crate::archive::extract_game(archive, &mut show)
    }

    /// Wait for input and turn it into an action.
    fn next_act(&mut self) -> Act {
        loop {
            if let Some((a, used)) = self.parse() {
                self.pending.drain(..used);
                return a;
            }
            let r = crate::sys::poll(500, self.mice.as_ref().map_or(-1, |m| m.fd()));
            if r.hangup {
                return Act::Cancel;
            }
            if !r.input && !r.extra {
                // redraw on resize
                return Act::None;
            }
            if r.extra {
                let (cols, rows) = term_size();
                let evs = self.mice.as_mut().map(|m| m.read((8, 16), cols, rows)).unwrap_or_default();
                for (btn, _x, y, press) in evs {
                    if let Some(a) = self.mouse(btn, y, press) {
                        return a;
                    }
                }
            }
            if r.input {
                let mut b = [0u8; 256];
                match crate::sys::read_stdin(&mut b) {
                    Ok(n) if n > 0 => self.pending.extend_from_slice(&b[..n]),
                    Err(e) if matches!(e.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) => {}
                    _ => return Act::Cancel,
                }
            }
        }
    }

    /// A mouse event at 1-based row `y`.
    fn mouse(&self, btn: u16, y: u16, press: bool) -> Option<Act> {
        match btn & !(4 | 8 | 16) {
            64 => Some(Act::Move(-3)),
            65 => Some(Act::Move(3)),
            0 if press => {
                let row = (y as usize).checked_sub(3)?; // list starts on screen row 3
                (row < self.list_rows()).then_some(Act::Click(self.top + row))
            }
            _ => None,
        }
    }

    /// Decode one action from `pending`: (action, bytes used), or None if
    /// more bytes are needed.
    fn parse(&self) -> Option<(Act, usize)> {
        let b = &self.pending;
        let first = *b.first()?;
        if first != 0x1b {
            let a = match first {
                b'\r' | b'\n' => Act::Activate,
                0x7f | 0x08 | b'h' => Act::Up,
                b'q' | 0x03 => Act::Cancel,
                b'k' => Act::Move(-1),
                b'j' => Act::Move(1),
                b'l' => Act::Open,
                b'~' => Act::Home,
                b'.' => Act::ToggleHidden,
                b'g' => Act::Top,
                b'G' => Act::Bottom,
                c if c.is_ascii_alphabetic() => Act::Letter(c as char),
                _ => Act::None,
            };
            return Some((a, 1));
        }
        if b.len() == 1 {
            // lone ESC unless more bytes arrive right away
            return if crate::sys::poll(30, -1).input { None } else { Some((Act::Cancel, 1)) };
        }
        if b[1] != b'[' {
            return Some((Act::None, 2));
        }
        // X10 mouse: ESC [ M b x y
        if b.get(2) == Some(&b'M') {
            if b.len() < 6 {
                return None;
            }
            let (cb, y) = (b[3].wrapping_sub(32) as u16, b[5].wrapping_sub(32) as u16);
            let release = cb & 3 == 3 && cb & (32 | 64) == 0;
            return Some((self.mouse(if release { 3 } else { cb }, y, !release).unwrap_or(Act::None), 6));
        }
        let end = b[2..].iter().position(|c| (0x40..=0x7e).contains(c))? + 2;
        let params = String::from_utf8_lossy(&b[2..end]).to_string();
        let a = match (b[end], params.as_str()) {
            (b'A', _) => Act::Move(-1),
            (b'B', _) => Act::Move(1),
            (b'C', _) => Act::Open,
            (b'D', _) => Act::Up,
            (b'H', _) => Act::Top,
            (b'F', _) => Act::Bottom,
            (b'~', "3") => Act::Delete,
            (b'~', "5") => Act::Page(-1),
            (b'~', "6") => Act::Page(1),
            (b'~', "1") | (b'~', "7") => Act::Top,
            (b'~', "4") | (b'~', "8") => Act::Bottom,
            // SGR mouse ESC [ < b ; x ; y M/m, urxvt ESC [ b ; x ; y M
            (b'M' | b'm', p) => {
                let sgr = p.starts_with('<');
                let v: Vec<u16> = p.trim_start_matches('<').split(';').filter_map(|x| x.parse().ok()).collect();
                match v.as_slice() {
                    [cb, _x, y] if sgr => self.mouse(*cb, *y, b[end] == b'M').unwrap_or(Act::None),
                    [cb, _x, y] => {
                        let cb = cb.saturating_sub(32);
                        let release = cb & 3 == 3 && cb & (32 | 64) == 0;
                        self.mouse(cb, *y, !release).unwrap_or(Act::None)
                    }
                    _ => Act::None,
                }
            }
            _ => Act::None,
        };
        Some((a, end + 1))
    }
}

// ------------------------------------------------------------ remembering

/// The game folder chosen last time, if it still looks like a game.
pub fn last_game() -> Option<PathBuf> {
    crate::settings::Settings::load().last.filter(|p| is_game_dir(p))
}

/// Remember `game` for the next run without arguments (and in the library).
pub fn remember(game: &Path) {
    let mut st = crate::settings::Settings::load();
    st.played(game);
    st.save();
}

/// How many games the library lists (folders still there).
pub fn library_size() -> usize {
    crate::settings::Settings::load().library.iter().filter(|g| is_game_dir(g)).count()
}

// ------------------------------------------------------------- installing

/// `--install`: copy this binary to the user binary folder.
pub fn install() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find this program: {}", e))?;
    install_from(&exe)
}

/// Can we write into `d` (an existing folder)?
fn writable(d: &Path) -> bool {
    crate::sys::dir_writable(d)
}

/// Where `--install` puts the program: a folder on the default `$PATH`, so
/// `emuera-tty` runs from any terminal right away. In order:
/// 1. a writable PATH folder that already has emuera-tty (upgrade in place);
/// 2. the usual user folders when they are on PATH: `$XDG_BIN_HOME`,
///    `~/.local/bin`, `~/bin`;
/// 3. any other writable PATH folder (e.g. `/usr/local/bin` as root), but
///    not tool-managed ones like `~/.cargo/bin`;
/// 4. failing all that, `$XDG_BIN_HOME` or `~/.local/bin` (created; the
///    install message then says how to add it to PATH).
pub fn bin_dir() -> Result<PathBuf, String> {
    let home = crate::sys::home_dir().ok_or("HOME is not set")?;
    let xdg = std::env::var_os("XDG_BIN_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
    let path: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).filter(|d| d.is_absolute()).collect()).unwrap_or_default();
    if let Some(d) = path.iter().find(|d| d.join(crate::sys::EXE_NAME).is_file() && writable(d)) {
        return Ok(d.clone());
    }
    let usual: Vec<PathBuf> = xdg.iter().cloned().chain([home.join(".local").join("bin"), home.join("bin")]).collect();
    if let Some(d) = usual.iter().find(|u| path.contains(u) && (writable(u) || !u.exists())) {
        return Ok(d.clone());
    }
    let managed = [".cargo", ".rustup", ".npm", ".nvm", ".pyenv", ".rbenv", ".local/share", "go/bin", "snap"];
    let is_managed = |d: &Path| d.strip_prefix(&home).is_ok_and(|r| managed.iter().any(|m| r.starts_with(m)));
    if let Some(d) = path.iter().find(|d| writable(d) && !is_managed(d)) {
        return Ok(d.clone());
    }
    Ok(xdg.unwrap_or_else(|| home.join(".local").join("bin")))
}

/// Copy the binary `exe` to the user binary folder ([`bin_dir`]).
fn install_from(exe: &Path) -> Result<String, String> {
    let dir = bin_dir()?;
    let dest = dir.join(crate::sys::EXE_NAME);
    if dest.canonicalize().ok() == exe.canonicalize().ok() {
        return Ok(format!("{} is already installed.", dest.display()));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {}", dir.display(), e))?;
    // write beside the target and rename, so replacing a running copy is safe
    let tmp = dir.join(".emuera-tty.new");
    // Windows can't replace a running program, but can rename it aside
    #[cfg(windows)]
    if dest.exists() {
        let old = dir.join(".emuera-tty.old.exe");
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::rename(&dest, &old);
    }
    std::fs::copy(exe, &tmp).map_err(|e| format!("{}: {}", tmp.display(), e))?;
    crate::sys::set_executable(&tmp).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &dest).map_err(|e| format!("{}: {}", dest.display(), e))?;
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir));
    let mut msg = format!("Installed {}.", dest.display());
    match install_man(&dir) {
        Ok(Some(p)) => msg += &format!("\nManual page: {} (man emuera-tty).", p.display()),
        Ok(None) => {}
        Err(e) => msg += &format!("\n(manual page not installed: {})", e),
    }
    if !on_path {
        if cfg!(windows) {
            msg += &format!("\n{} is not on your PATH; add it with (then open a new terminal):\n  setx PATH \"%PATH%;{}\"", dir.display(), dir.display());
        } else {
            msg += &format!("\n{} is not on your PATH; add this to ~/.bashrc or ~/.profile:\n  export PATH=\"{}:$PATH\"", dir.display(), dir.display());
        }
    }
    Ok(msg)
}

/// The manual page, built in.
pub const MANUAL: &str = include_str!("../doc/emuera-tty.1");

/// Where the manual page goes for a program in `bin`: `bin/../share/man/
/// man1`, which man-db and mandoc search for every PATH folder.
fn man_path(bin: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    Some(bin.parent()?.join("share").join("man").join("man1").join("emuera-tty.1"))
}

/// Install the manual page next to the program in `bin`.
fn install_man(bin: &Path) -> Result<Option<PathBuf>, String> {
    let Some(p) = man_path(bin) else { return Ok(None) };
    let d = p.parent().unwrap();
    std::fs::create_dir_all(d).map_err(|e| format!("{}: {}", d.display(), e))?;
    std::fs::write(&p, MANUAL).map_err(|e| format!("{}: {}", p.display(), e))?;
    Ok(Some(p))
}

/// `--manual`: show the manual page (through man when there is one).
pub fn show_manual() {
    use std::io::Write as _;
    if crate::sys::is_tty() && which("man").is_some() {
        let child = std::process::Command::new("man").args(["-l", "-"]).stdin(std::process::Stdio::piped()).spawn();
        if let Ok(mut c) = child {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(MANUAL.as_bytes());
            }
            if c.wait().is_ok_and(|s| s.success()) {
                return;
            }
        }
    }
    print!("{}", MANUAL);
}

/// `--uninstall`: remove the installed copy.
pub fn uninstall() -> Result<String, String> {
    // the copy the shell would run, else where --install would put it
    let on_path = std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join(crate::sys::EXE_NAME)).find(|f| f.is_file() && f.parent().is_some_and(writable)));
    let dest = match on_path {
        Some(f) => f,
        None => bin_dir()?.join(crate::sys::EXE_NAME),
    };
    match std::fs::remove_file(&dest) {
        Ok(()) => {
            if let Some(m) = dest.parent().and_then(man_path) {
                let _ = std::fs::remove_file(m);
            }
            Ok(format!("Removed {}.", dest.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(format!("{} is not installed.", dest.display())),
        Err(e) => Err(format!("{}: {}", dest.display(), e)),
    }
}

/// `--reset-data`: forget emuera-tty's own settings (the remembered game
/// folder). Game saves live in each game's `sav/` and are not touched.
pub fn reset_data() -> Result<String, String> {
    let cfg = crate::settings::config_path().ok_or("HOME is not set")?;
    let dir = cfg.parent().map(Path::to_path_buf).unwrap_or(cfg);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(format!("Removed {} (settings and the game list); the next start will ask for a game folder.", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("Nothing to reset.".into()),
        Err(e) => Err(format!("{}: {}", dir.display(), e)),
    }
}

/// Where `--upgrade` keeps its checkout: `$XDG_DATA_HOME/emuera-tty/src`.
fn source_dir() -> Option<PathBuf> {
    Some(crate::sys::data_base()?.join("emuera-tty").join("src"))
}

pub const REPO_URL: &str = "https://github.com/astartes-gaming/emuera-tty.git";

/// Run a command, showing its output; error text if it fails.
fn run_cmd(cmd: &mut std::process::Command) -> Result<(), String> {
    let shown = format!("{:?}", cmd);
    eprintln!("$ {}", shown.replace('"', ""));
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{} failed ({})", shown, s)),
        Err(e) => Err(format!("{}: {}", shown, e)),
    }
}

/// `--upgrade`: fetch the latest source (git clone / pull), build it with
/// cargo and install the result to the user binary folder. Needs git and cargo.
pub fn upgrade() -> Result<String, String> {
    use std::process::Command;
    let src = source_dir().ok_or("HOME is not set")?;
    if src.join(".git").is_dir() {
        run_cmd(Command::new("git").arg("-C").arg(&src).args(["pull", "--ff-only"]))?;
    } else {
        if let Some(p) = src.parent() {
            std::fs::create_dir_all(p).map_err(|e| format!("{}: {}", p.display(), e))?;
        }
        run_cmd(Command::new("git").args(["clone", "--depth", "1", REPO_URL]).arg(&src))?;
    }
    // cargo from PATH, else rustup's default location
    let cargo = crate::sys::home_dir()
        .map(|h| h.join(".cargo").join("bin").join(if cfg!(windows) { "cargo.exe" } else { "cargo" }))
        .filter(|c| c.is_file() && which("cargo").is_none())
        .unwrap_or_else(|| PathBuf::from("cargo"));
    run_cmd(Command::new(cargo).arg("build").arg("--release").arg("--manifest-path").arg(src.join("Cargo.toml")))?;
    let built = src.join("target").join("release").join(crate::sys::EXE_NAME);
    install_from(&built)
}

/// The git checkout a game folder belongs to: the folder itself or one of
/// the two above it (a game kept in a subfolder of its repository), never
/// further up, so a home folder under git isn't taken for the game's.
fn git_root(game: &Path) -> Option<PathBuf> {
    which("git")?;
    game.ancestors().take(3).find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// `git pull --ff-only` in `root`, without prompts (no password or editor
/// can be asked for here). Ok: what changed; Err: why it didn't.
fn git_pull(root: &Path) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let git = |args: &[&str]| {
        let mut c = Command::new("git");
        c.arg("-C").arg(root).args(args).stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            c.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        c.output().map_err(|e| format!("git: {}", e))
    };
    let head = |o: std::process::Output| String::from_utf8_lossy(&o.stdout).trim().to_string();
    let before = head(git(&["rev-parse", "HEAD"])?);
    let o = git(&["pull", "--ff-only"])?;
    if !o.status.success() {
        // git's own explanation (its error: / fatal: line and the files
        // listed under it), not the fetch progress or hints
        let err = String::from_utf8_lossy(&o.stderr);
        let lines: Vec<&str> = err.lines().collect();
        let why = match lines.iter().position(|l| l.starts_with("error:") || l.starts_with("fatal:")) {
            Some(k) => {
                let mut w = lines[k].split_once(':').map_or("", |x| x.1).trim().to_string();
                for f in lines[k + 1..].iter().take_while(|l| l.starts_with('\t') || l.starts_with("  ")).take(3) {
                    w += " ";
                    w += f.trim();
                }
                w
            }
            None => lines.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty() && !l.starts_with("hint:")).unwrap_or("").to_string(),
        };
        return Err(format!("Update failed: {}", if why.is_empty() { "git pull didn't work".to_string() } else { why }));
    }
    let after = head(git(&["rev-parse", "HEAD"])?);
    if before == after {
        return Ok("Already up to date.".into());
    }
    let n = head(git(&["rev-list", "--count", &format!("{}..{}", before, after)])?);
    let files = head(git(&["diff", "--shortstat", &before, &after])?);
    Ok(format!("Updated: {} new commit{} ({}).", n, if n == "1" { "" } else { "s" }, files))
}

/// First `name` on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{}.exe", name) } else { name.to_string() };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())
}
