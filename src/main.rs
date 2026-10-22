//! emuera-tty: run Emuera (ERB) games in a terminal.
//!
//! Startup: parse options, read CSVs (`csvdata`), parse every .ERH then
//! .ERB file (`parser`), load image resources, pick a frontend (full-screen
//! `tty` or `--headless`), read the game's config files, and hand over to
//! the VM's system loop on a thread with a large stack (ERB code can
//! recurse deeply, and each ERB call is a Rust call).

mod archive;
mod ast;
mod atomic64;
mod clipboard;
mod config_names;
mod console;
mod csvdata;
mod emuconfig;
mod expr;
mod frontend;
mod fxhash;
mod gfx;
mod html;
mod lint;
mod loader;
#[cfg(target_os = "linux")]
mod mice;
#[cfg(not(target_os = "linux"))]
#[path = "mice_none.rs"]
mod mice;
mod mt;
mod parser;
mod picker;
mod regex;
mod resources;
mod retro;
mod savemgr;
mod settings;
mod sound;
mod sys;
mod terminfo;
mod translate;
mod tty;
mod vm;
mod xml;

use std::path::PathBuf;

/// Command-line options.
struct Opts {
    /// None: use the current folder, the remembered game, or the picker
    game: Option<PathBuf>,
    /// --choose: open the picker even if a game is remembered
    choose: bool,
    headless: bool,
    gfx: String,
    strict: bool,
    check: bool,
    debug: bool,
    echo: bool,
    /// --retro / --retro=line: the retro-terminal frontend (otherwise used
    /// only when $TERM isn't ANSI)
    retro: Option<String>,
    /// --term: the terminfo name for the retro / serial terminal
    term: Option<String>,
    /// --serial DEVICE, with --baud / --serial-format / --flow
    serial: Option<String>,
    baud: u32,
    serial_format: String,
    flow: String,
    /// --size COLSxROWS
    size: Option<(usize, usize)>,
    /// --saves / --backup-saves
    saves: Option<&'static str>,
}

/// Print help; exit 0 when asked for (--help), 2 on bad options.
fn usage_exit(code: i32) -> ! {
    eprintln!(
        "usage: emuera-tty [GAME_DIR] [options]\n\
         \n\
         GAME_DIR may be a game folder or an archive of one (.zip, .tar.gz, .7z…;\n\
         extracted once to ~/.local/share/emuera-tty/games).\n\
         Without GAME_DIR: the current folder if it is a game, else the game\n\
         chosen last time, else a folder picker opens.\n\
         \n\
         options:\n\
         \x20 --choose                         pick a game folder (even if one is remembered)\n\
         \x20 --gfx auto|kitty|iterm|sixel|fb|none image backend (default auto)\n\
         \x20 --headless                       plain stdin/stdout mode (scripts, tests)\n\
         \x20 --echo                           echo inputs in headless mode\n\
         \x20 --check                          parse the game and report errors only\n\
         \x20 --strict                         unknown commands are errors\n\
         \x20 --debug                          enable [IF_DEBUG] / ;#; lines\n\
         \n\
         saves (also in the F1 overlay: Manage saves):\n\
         \x20 --saves                          list the game's saves with their comments\n\
         \x20 --backup-saves                   copy all of them to a new backup folder\n\
         \n\
         retro and serial terminals:\n\
         \x20 --retro[=line]                   drive the terminal from its terminfo entry, ASCII only\n\
         \x20                                  (automatic when $TERM isn't ANSI: vt52, adm3a, dumb…);\n\
         \x20                                  =line prints line by line, for terminals with no cursor control\n\
         \x20 --term NAME                      terminfo name of the terminal (default $TERM; vt100 on --serial)\n\
         \x20 --serial DEVICE                  play on a terminal on a serial line (/dev/ttyS0, /dev/ttyUSB0…)\n\
         \x20 --baud N                         line speed (default 9600)\n\
         \x20 --serial-format 8N1|7E1|…        data bits, parity, stop bits (default 8N1)\n\
         \x20 --flow xon|rts|none              flow control (default xon: XON/XOFF)\n\
         \x20 --size COLSxROWS                 screen size if the terminal can't report it (default: terminfo)\n\
         \n\
         setup:\n\
         \x20 --install                        copy this program to a folder on your PATH (~/.local/bin…)\n\
         \x20 --upgrade                        fetch, build (git + cargo) and install the latest version\n\
         \x20 --uninstall                      remove the installed copy\n\
         \x20 --reset-data                     forget settings and the game library; saves are kept\n\
         \x20 --manual                         the full manual (also: man emuera-tty once installed)"
    );
    std::process::exit(code)
}

fn usage() -> ! {
    usage_exit(2)
}

/// Parse `std::env::args` (exits with usage on unknown options).
fn parse_args() -> Opts {
    let mut o = Opts {
        game: None,
        choose: false,
        headless: false,
        gfx: "auto".into(),
        strict: false,
        check: false,
        debug: false,
        echo: false,
        retro: None,
        term: None,
        serial: None,
        baud: 9600,
        serial_format: "8N1".into(),
        flow: "xon".into(),
        size: None,
        saves: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => o.headless = true,
            "--strict" => o.strict = true,
            "--check" => o.check = true,
            "--debug" => o.debug = true,
            "--echo" => o.echo = true,
            "--choose" => o.choose = true,
            "--saves" => o.saves = Some("list"),
            "--backup-saves" => o.saves = Some("backup"),
            "--install" | "--uninstall" | "--upgrade" | "--reset-data" => {
                let r = match a.as_str() {
                    "--install" => picker::install(),
                    "--uninstall" => picker::uninstall(),
                    "--upgrade" => picker::upgrade(),
                    _ => picker::reset_data(),
                };
                match r {
                    Ok(m) => {
                        println!("{}", m);
                        std::process::exit(0)
                    }
                    Err(e) => {
                        eprintln!("emuera-tty: {}", e);
                        std::process::exit(1)
                    }
                }
            }
            "--gfx" => o.gfx = it.next().unwrap_or_else(|| usage()),
            "--retro" => o.retro = Some("screen".into()),
            s if s.starts_with("--retro=") => o.retro = Some(s[8..].to_string()),
            "--term" => o.term = Some(it.next().unwrap_or_else(|| usage())),
            "--serial" => o.serial = Some(it.next().unwrap_or_else(|| usage())),
            "--baud" => o.baud = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--serial-format" => o.serial_format = it.next().unwrap_or_else(|| usage()),
            "--flow" => o.flow = it.next().unwrap_or_else(|| usage()),
            "--size" => {
                let v = it.next().unwrap_or_else(|| usage());
                o.size = v.split_once(['x', 'X']).and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)));
                if o.size.is_none() {
                    usage();
                }
            }
            "-h" | "--help" => usage_exit(0),
            "--manual" | "--man" => {
                picker::show_manual();
                std::process::exit(0)
            }
            s if s.starts_with("--gfx=") => o.gfx = s[6..].to_string(),
            s if s.starts_with('-') => usage(),
            s => o.game = Some(PathBuf::from(s)),
        }
    }
    if let Ok(g) = std::env::var("EMUERA_GFX") {
        if o.gfx == "auto" {
            o.gfx = g;
        }
    }
    o
}

/// Read and parse the game's ERH / ERB files (on all cores): the program
/// and how many files there were. Also used by the reload-ERB hotkey.
/// `lazy`: function bodies are compiled when first called (less memory,
/// quicker start); else all now (`--check`).
fn build_program(
    game: &std::path::Path,
    erb_dir: Option<&std::path::Path>,
    rename: &[(String, String)],
    debug: bool,
    lazy: bool,
    phase: &mut dyn FnMut(&str),
) -> (parser::Program, usize) {
    let mut p = parser::Parser::new(rename, debug);
    // EMUERA_THREADS=n limits the worker threads (default: all cores)
    let cores = std::env::var("EMUERA_THREADS")
        .ok()
        .and_then(|n| n.parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let (erhs, erbs) = match erb_dir {
        Some(erb) => {
            // Emuera's order (it decides which of a duplicate wins, the
            // order of event functions, and of #DIMs in saves)
            let l = loader::Listing::of(game);
            let (erhs, erbs) = (loader::emuera_files(erb, &["erh"], l), loader::erb_files(erb, l));
            if !l.subdirs {
                let skipped = loader::walk(erb, &["erb", "erh"]).len().saturating_sub(erhs.len() + erbs.len());
                if skipped > 0 {
                    eprintln!("note: {} ERB/ERH files in subfolders not loaded: the game's config doesn't turn on サブディレクトリを検索する (Search subfolders), so Emuera skips them too", skipped);
                }
            }
            (erhs, erbs)
        }
        None => (Vec::new(), Vec::new()),
    };
    let rel = |f: &std::path::Path| f.strip_prefix(game).unwrap_or(f).display().to_string();
    for f in erhs.iter().chain(&erbs) {
        p.prog.files.push(rel(f));
    }
    // headers first: what they declare holds in every .ERB
    let texts: Vec<Result<String, String>> = parallel_map(&erhs, cores, |f| loader::read_text(f).map_err(|e| format!("{}: {}", f.display(), e)));
    phase("read erh");
    for (i, (f, t)) in erhs.iter().zip(texts).enumerate() {
        match t {
            Ok(text) => p.parse_erh(i as u32, &parser::SourceFile { path: rel(f), text }),
            Err(e) => eprintln!("{}", e),
        }
    }
    phase("parse erh");
    let n_files = erhs.len() + erbs.len() + 1;
    if lazy {
        p.compile_lazily(n_files);
    }
    // ERB files are independent once the headers are known: read and parse
    // them on all cores in contiguous chunks (a file at a time per core),
    // then merge in file order
    let items: Vec<(u32, &std::path::PathBuf, usize)> =
        erbs.iter().enumerate().map(|(j, f)| ((erhs.len() + j) as u32, f, std::fs::metadata(f).map_or(1, |m| m.len() as usize))).collect();
    let forks: Vec<parser::Program> = parallel_chunks(
        &items,
        cores,
        |it| it.2,
        |chunk| {
            let mut q = p.fork();
            for (i, f, _) in chunk {
                match std::fs::read(f) {
                    Ok(raw) => q.parse_erb_file(*i, &rel(f), f, &raw),
                    Err(e) => eprintln!("{}: {}", f.display(), e),
                }
            }
            q.prog
        },
    );
    for prog in forks {
        p.absorb(prog);
    }
    // debugging aid: EMUERA_INJECT="STATEMENT||STATEMENT…" becomes a
    // function @__INJECT the VM runs at input number EMUERA_INJECT_AT
    if let Ok(code) = std::env::var("EMUERA_INJECT") {
        let src = parser::SourceFile { path: "(EMUERA_INJECT)".into(), text: format!("@__INJECT\n{}\n", code.replace("||", "\n")) };
        p.prog.files.push(src.path.clone());
        p.parse_erb((n_files - 1) as u32, &src);
    }
    phase("parse erb");
    (p.prog, n_files - 1)
}

/// Write the sampling profile: time per Rust function, by itself and with
/// what it calls.
#[cfg(feature = "profile")]
fn write_pprof(path: &str, g: &pprof::ProfilerGuard) {
    let Ok(r) = g.report().build() else { return };
    let mut own: std::collections::HashMap<String, isize> = Default::default();
    let mut incl: std::collections::HashMap<String, isize> = Default::default();
    let mut edges: std::collections::HashMap<String, isize> = Default::default();
    let mut total = 0isize;
    for (frames, n) in &r.data {
        total += n;
        let names: Vec<String> = frames.frames.iter().flat_map(|f| f.iter().map(|s| s.name())).collect();
        if let Some(l) = names.first() {
            *own.entry(l.clone()).or_default() += n;
        }
        // caller → callee edges (frames run innermost first)
        let mut seen_e = std::collections::HashSet::new();
        for w in names.windows(2) {
            let e = format!("{}  <-  {}", w[0], w[1]);
            if seen_e.insert(e.clone()) {
                *edges.entry(e).or_default() += n;
            }
        }
        let mut seen = std::collections::HashSet::new();
        for nm in names {
            if seen.insert(nm.clone()) {
                *incl.entry(nm).or_default() += n;
            }
        }
    }
    let top = |m: &std::collections::HashMap<String, isize>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        v.into_iter().take(80).map(|(k, n)| format!("{:6.2}% {:7} {}\n", *n as f64 * 100.0 / total.max(1) as f64, n, k)).collect::<String>()
    };
    let _ = std::fs::write(path, format!("{} samples\n== self\n{}\n== inclusive\n{}\n== calls\n{}", total, top(&own), top(&incl), top(&edges)));
}

/// "  rss 123 MB" (Linux), for EMUERA_TIMING.
fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .map_or(String::new(), |kb| format!("  rss {} MB", kb / 1024))
}

/// Load the game and run it; returns the process exit code.
fn run(o: Opts) -> i32 {
    let Some(game) = choose_game(&o) else { return 0 };
    // development builds (--features profile): EMUERA_PPROF=<file>
    #[cfg(feature = "profile")]
    let _pprof = std::env::var("EMUERA_PPROF")
        .ok()
        .map(|f| (f, pprof::ProfilerGuardBuilder::default().frequency(4000).blocklist(&["libc", "libgcc", "pthread", "vdso"]).build().unwrap()));
    if let Some(cmd) = o.saves {
        return saves_cmd(cmd, &game);
    }
    let Some(csv_dir) = loader::find_dir_ci(&game, "CSV") else {
        eprintln!("{}: no CSV directory found", game.display());
        return 1;
    };
    let erb_dir = loader::find_dir_ci(&game, "ERB");
    let t0 = std::time::Instant::now();
    // EMUERA_TIMING=1: report how long each startup phase takes
    let timing = std::env::var_os("EMUERA_TIMING").is_some();
    let mut lap = std::time::Instant::now();
    let mut phase = |name: &str| {
        if timing {
            eprintln!("timing: {:<12} {:>8.2?}{}", name, lap.elapsed(), rss());
        }
        lap = std::time::Instant::now();
    };
    let csv = csvdata::GameCsv::load(&csv_dir, erb_dir.as_deref());
    phase("csv");
    for w in &csv.warnings {
        eprintln!("csv: {}", w);
    }
    // function bodies are compiled when first called, except for --check
    // (or EMUERA_PARSE=all): a game runs a few percent of them
    let lazy = !o.check && std::env::var("EMUERA_PARSE").map_or(true, |v| v != "all");
    let (prog, n_files) = build_program(&game, erb_dir.as_deref(), &csv.rename, o.debug, lazy, &mut phase);
    let n_funcs: usize = prog.funcs.values().map(|v| v.len()).sum();
    eprintln!(
        "loaded {} files, {} functions, {} chara templates in {:.2?} ({} parse errors)",
        n_files,
        n_funcs,
        csv.charas.len(),
        t0.elapsed(),
        prog.errors.len()
    );
    for e in prog.errors.iter().take(if o.check { usize::MAX } else { 30 }) {
        eprintln!("parse: {}", e);
    }
    if prog.errors.len() > 30 && !o.check {
        eprintln!("... {} more parse errors (run with --check to list all)", prog.errors.len() - 30);
    }
    if o.check {
        if std::env::var_os("EMUERA_LINT").is_some() {
            for l in lint::lint(&prog) {
                eprintln!("lint: {}", l);
            }
        }
        let code = if prog.errors.is_empty() { 0 } else { 1 };
        #[cfg(feature = "profile")]
        if let Some((f, g)) = &_pprof {
            write_pprof(f, g);
        }
        // the process exits next: don't spend time freeing ~100k functions
        std::mem::forget(prog);
        return code;
    }
    // the parse threads leave most of their scratch memory (source text,
    // line copies) cached in glibc's per-thread arenas: give it back
    sys::trim_memory();
    let res = resources::Resources::load(loader::find_dir_ci(&game, "resources").as_deref());
    // saved settings (options and EMUERA_* variables win)
    let st = settings::Settings::load();
    st.export_env();
    let mut o = o;
    if o.gfx == "auto" && std::env::var_os("EMUERA_GFX").is_none() {
        o.gfx = st.get("images").to_string();
    }
    let term_env = std::env::var("TERM").unwrap_or_default();
    let use_retro = !o.headless
        && (o.serial.is_some() || o.retro.is_some() || st.get("frontend") == "retro" || (st.get("frontend") != "ansi" && retro::wanted_for(&term_env)));
    let fe: Box<dyn frontend::Frontend> = if o.headless {
        Box::new(frontend::Headless::new(o.echo))
    } else if use_retro {
        let port = match &o.serial {
            Some(dev) => retro::Port::serial(dev, &retro::SerialOpts { baud: o.baud, format: o.serial_format.clone(), flow: o.flow.clone() }),
            None => retro::Port::local(),
        };
        let term = o.term.clone().unwrap_or_else(|| if o.serial.is_some() || term_env.is_empty() { "vt100".into() } else { term_env.clone() });
        let r = port.and_then(|p| retro::Retro::new(p, &term, o.size, o.retro.as_deref() == Some("line")));
        match r {
            Ok(r) => {
                if let Some(dev) = &o.serial {
                    eprintln!("emuera-tty: playing on {} ({}, {} baud, {} mode)", dev, r.term_name(), o.baud, r.mode_name());
                }
                Box::new(r)
            }
            Err(e) => {
                eprintln!("emuera-tty: {}", e);
                return 1;
            }
        }
    } else {
        match tty::Tty::new(&o.gfx) {
            Ok(mut t) => {
                t.contrast = st.contrast_ratio();
                t.set_recolor(&std::env::var("EMUERA_RECOLOR").unwrap_or_else(|_| st.get("recolor").to_string()));
                t.wheel = st.get("wheel").parse().unwrap_or(3);
                t.status_info = st.get("statusbar") == "on";
                t.show_memory = st.get("memory") == "on";
                t.set_kbd_pointer(st.get("kbdpointer"));
                if st.get("translator") != "off" {
                    let note = t.set_translate(&st);
                    eprintln!("{}", note);
                }
                t.clip_mode = st.get("clipboard").to_string();
                Box::new(t)
            }
            Err(e) => {
                eprintln!("terminal init failed ({}); falling back to headless", e);
                Box::new(frontend::Headless::new(o.echo))
            }
        }
    };
    let rename = csv.rename.clone();
    let mut vm = vm::Vm::new(prog, csv, res, fe, game.clone());
    // the reload hotkey parses the ERB files again
    {
        let (g, erb, debug) = (game.clone(), erb_dir.clone(), o.debug);
        vm.reloader = Some(Box::new(move || build_program(&g, erb.as_deref(), &rename, debug, lazy, &mut |_| {}).0));
    }
    if timing {
        eprintln!("timing: {:<12} {:>8.2?}{} (resources, terminal, data init)", "vm ready", lap.elapsed(), rss());
    }
    if let Some(px) = emuconfig::load(&game, &csv_dir, &mut vm.con, &mut vm.text_exts, &mut vm.save_opts) {
        // no real terminal to fit: use Emuera's width (half-width cells are font_size/2 px)
        if o.headless {
            vm.fe.set_cols((px / (vm.con.font_size / 2.0).max(1.0)) as usize);
            vm.con.width = vm.fe.cols().saturating_sub(1).max(20);
        }
    }
    vm.strict = o.strict;
    // EMUERA_TRANSLATE_LOG: lines remember where they were printed from
    if std::env::var_os("EMUERA_TRANSLATE_LOG").is_some() {
        vm.con.track_src = true;
        vm.con.src_files = vm.prog.files.clone();
    }
    // sound: in a terminal, through a player program (see sound.rs);
    // EMUERA_SOUND=on/off and EMUERA_SOUND_PLAYER=mpv win over the setting,
    // which the settings page can change while playing
    if !o.headless {
        vm.con.sound_on = sound::wanted(st.get("sound"));
        let named = std::env::var("EMUERA_SOUND_PLAYER").ok();
        vm.sound = sound::Player::find(named.as_deref()).map(sound::Sound::new);
        if let Some(s) = vm.sound.as_mut() {
            s.set_on(vm.con.sound_on);
        }
    }
    let sd = vm.save_dir();
    vm.fe.set_game(&game, &sd);
    let r = vm.run();
    vm.fe.shutdown();
    #[cfg(feature = "profile")]
    if let Some((f, g)) = &_pprof {
        write_pprof(f, g);
    }
    if vm.defer_warnings {
        for w in &vm.log {
            eprintln!("warning: {}", w);
        }
    }
    for w in vm.res.warnings.iter().take(10) {
        eprintln!("resources: {}", w);
    }
    // the process exits next; freeing the program and all game data one
    // allocation at a time would only delay quitting
    std::mem::forget(vm);
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", e);
            1
        }
    }
}

/// Run `f` over contiguous chunks of `items` on up to `threads` threads and
/// return the results in chunk order. Chunks are cut so each holds about
/// the same `weight` (e.g. bytes of source), several per thread, and idle
/// threads take the next chunk from a shared counter, so one huge file
/// doesn't leave the other cores waiting. Threads get a big stack
/// (parsing recurses).
fn parallel_chunks<T: Sync, R: Send>(items: &[T], threads: usize, weight: impl Fn(&T) -> usize, f: impl Fn(&[T]) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    if items.is_empty() {
        return Vec::new();
    }
    let threads = threads.max(1);
    // cut into ~4 chunks per thread of similar total weight
    let total: usize = items.iter().map(&weight).sum::<usize>().max(1);
    let target = total.div_ceil(threads * 4).max(1);
    let mut bounds = vec![0];
    let mut acc = 0;
    for (i, it) in items.iter().enumerate() {
        acc += weight(it);
        if acc >= target && i + 1 < items.len() {
            bounds.push(i + 1);
            acc = 0;
        }
    }
    bounds.push(items.len());
    let chunks: Vec<&[T]> = bounds.windows(2).map(|w| &items[w[0]..w[1]]).collect();
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..chunks.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..threads.min(chunks.len()) {
            std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn_scoped(s, || loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = chunks.get(k) else { break };
                    let r = f(c);
                    results.lock().unwrap()[k] = Some(r);
                })
                .expect("spawn");
        }
    });
    results.into_inner().unwrap().into_iter().map(|r| r.expect("chunk not processed")).collect()
}

/// `f` applied to every item, using up to `threads` threads; order kept.
fn parallel_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    parallel_chunks(items, threads, |_| 1, |c| c.iter().map(&f).collect::<Vec<R>>()).into_iter().flatten().collect()
}

/// `--saves` / `--backup-saves`.
fn saves_cmd(cmd: &str, game: &std::path::Path) -> i32 {
    let dir = savemgr::save_dir_of(game);
    let root = savemgr::backup_root(game).unwrap_or_else(|| dir.join("backups"));
    if cmd == "backup" {
        return match savemgr::backup_all(&dir, &root, "") {
            Ok((p, n)) => {
                println!("backed up {} files to {}", n, p.display());
                0
            }
            Err(e) => {
                eprintln!("emuera-tty: {}", e);
                1
            }
        };
    }
    println!("saves in {}:", dir.display());
    for s in savemgr::list(&dir) {
        println!("  {:<8} {:<16} {:>9}  {}", s.label(), savemgr::when(s.modified), s.size, s.comment);
    }
    let bks = savemgr::backups(&root);
    if !bks.is_empty() {
        println!("{} backups in {}", bks.len(), root.display());
    }
    0
}

/// Decide which game folder to run (see `usage`). None: the user cancelled
/// the picker (or there is nothing to run, already reported).
fn choose_game(o: &Opts) -> Option<PathBuf> {
    let interactive = !o.headless && !o.check && sys::is_tty();
    // an explicit folder is used as-is; only picker choices are remembered
    if let Some(g) = &o.game {
        if archive::is_archive(g) {
            if let Some(dir) = archive::already_extracted(g) {
                return Some(dir);
            }
            eprint!("Extracting {}…", g.display());
            let r = archive::extract_game(g, &mut |n| eprint!("\rExtracting {}… {} files", g.display(), n));
            eprintln!();
            return match r {
                Ok(dir) => {
                    eprintln!("Game folder: {}", dir.display());
                    Some(dir)
                }
                Err(e) => {
                    eprintln!("emuera-tty: {}", e);
                    None
                }
            };
        }
        // a game played by path joins the library too (not as the last one)
        if interactive && picker::is_game_dir(g) {
            let mut st = settings::Settings::load();
            st.add_game(g);
            st.save();
        }
        return Some(g.clone());
    }
    if !o.choose {
        if picker::is_game_dir(std::path::Path::new(".")) {
            return Some(PathBuf::from("."));
        }
        // one game known: play it; several: let the user choose
        if picker::library_size() < 2 || !interactive {
            if let Some(g) = picker::last_game() {
                return Some(g);
            }
        }
    }
    if !interactive {
        eprintln!("emuera-tty: no game folder given (pass GAME_DIR, or run in a terminal to pick one)");
        return None;
    }
    let g = picker::launcher()?;
    picker::remember(&g);
    Some(g)
}

fn main() {
    let o = parse_args();
    // ERB call chains can be deep; run on a thread with a large stack.
    let h = std::thread::Builder::new().stack_size(512 << 20).spawn(move || run(o)).expect("spawn");
    let code = h.join().unwrap_or(101);
    std::process::exit(code);
}
