//! Full-screen terminal frontend: raw-mode input with mouse support, a
//! redraw-per-frame renderer for the Emuera console, and inline images via
//! the kitty / sixel / framebuffer backends.
//!
//! Each frame is rebuilt from the [`Console`]: lines are laid out bottom-up
//! into screen rows (`layout_line`, which wraps and aligns), the visible
//! window is chosen from the scroll offset, and the whole screen is written
//! in one go. While drawing, the positions of buttons are recorded
//! (`btn_map`) so mouse clicks and hover can be mapped back to values.
//!
//! Input is read raw from stdin and decoded into [`Key`]s: UTF-8 text,
//! cursor/paging keys, and mouse reports in SGR, urxvt or X10 encoding.
//! On the Linux console the mouse comes from `/dev/input/mice` instead
//! (`crate::mice`). Colours are downgraded to 256 or 16 when the terminal
//! can't do 24-bit.

use crate::ast::Align;
use crate::console::{Console, Line, Rgb, Seg, Style};
use crate::frontend::{Frontend, InputKind, InputReq, InputResult};
use crate::gfx::{self, Backend, Placement};
use crate::resources::Resources;
use crate::vm::value::Value;
use std::io::Write;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

mod envvars;
mod gamecfg;
mod saves;

/// How many rows above / below the view are searched for images that
/// reach into it (portraits are about 15 rows tall).
const IMG_REACH: usize = 40;

/// How many colours the terminal can show (picked from TERM / COLORTERM).
#[derive(Clone, Copy, PartialEq)]
enum ColorMode {
    True,
    C256,
    C16,
}

/// One screen cell after layout; `w` is 2 for wide (CJK) characters.
#[derive(Clone)]
struct Cell {
    ch: char,
    w: u8,
    style: Style,
    button: Option<usize>,
}

/// An image anchored at a column of a screen row.
struct ImgAt {
    name: String,
    /// the button it belongs to and its `srcb` picture
    button: Option<usize>,
    hover: Option<String>,
    col: usize,
    w: f32,
    h: f32,
    ypos: f32,
}

/// Translate all text (menus too), not only dialog? Auto: unless the
/// game is shown in English.
fn all_text(st: &crate::settings::Settings) -> bool {
    match st.get("translate_what") {
        "all" => true,
        "auto" => st.get("translate") != "en",
        _ => false,
    }
}

/// Where a line was printed ("ERB/x.ERB:12"), for EMUERA_TRANSLATE_LOG.
fn place_of(l: &Line, files: &[String]) -> Option<String> {
    (l.src != 0).then(|| format!("{}:{}", files.get((l.src >> 32) as usize - 1).map_or("?", |f| f.as_str()), l.src & 0xFFFF_FFFF))
}

/// What identifies a line across frames (indices move when the scrollback
/// is trimmed): where it was printed, in which wait, and its text.
fn line_key(l: &Line) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    l.wait.hash(&mut h);
    l.src.hash(&mut h);
    for s in &l.segs {
        if let Seg::Text { text, .. } = s {
            text.hash(&mut h);
        }
    }
    h.finish()
}

/// `t` spread over lines as wide as `widths` say, in proportion: words
/// (characters, for text without spaces) go to the line whose share of
/// the whole they fall in.
fn spread(t: &str, widths: &[usize], width: impl Fn(&str) -> usize) -> Vec<String> {
    let n = widths.len().max(1);
    let total = widths.iter().sum::<usize>().max(1);
    // words; text written without spaces (Japanese, Chinese) by character
    let tokens: Vec<String> =
        if t.contains(' ') || t.is_ascii() { t.split_inclusive(' ').map(str::to_string).collect() } else { t.chars().map(|c| c.to_string()).collect() };
    let all = width(t).max(1);
    let mut out = vec![String::new(); n];
    let (mut k, mut done, mut share) = (0usize, 0usize, widths[0]);
    for tok in tokens {
        let w = width(&tok);
        // past this line's share of the translation (by its middle): next line
        while k + 1 < n && !out[k].trim().is_empty() && (2 * done + w) * total > 2 * share * all {
            k += 1;
            share += widths[k];
        }
        out[k].push_str(&tok);
        done += w;
    }
    out.iter().map(|s| s.trim().to_string()).collect()
}

/// `s` broken into lines of at most `w` columns at spaces (a word longer
/// than that is cut), `most` lines at most, the last ending in … if more
/// was left.
fn wrap_words(s: &str, w: usize, most: usize) -> Vec<String> {
    let cw = |s: &str| s.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>();
    let mut lines: Vec<String> = vec![String::new()];
    for word in s.split(' ') {
        let cur = lines.last_mut().unwrap();
        if !cur.is_empty() && cw(cur) + 1 + cw(word) > w {
            lines.push(String::new());
        }
        let cur = lines.last_mut().unwrap();
        if !cur.is_empty() {
            cur.push(' ');
        }
        for c in word.chars() {
            if cw(cur) + UnicodeWidthChar::width(c).unwrap_or(0) > w {
                break;
            }
            cur.push(c);
        }
    }
    if lines.len() > most {
        lines.truncate(most);
        let last = lines.last_mut().unwrap();
        while cw(last) + 1 > w {
            last.pop();
        }
        last.push('…');
    }
    lines
}

/// One laid-out screen row.
#[derive(Default)]
struct DRow {
    cells: Vec<Cell>,
    imgs: Vec<ImgAt>,
    /// `<div>` content anchored on this row, painted over the rows at its
    /// offset once the screen is laid out
    divs: Vec<DivAt>,
}

/// A laid-out `<div>`: its rows, drawn from column `col`, `dy` rows below
/// (negative: above) the row it is anchored on.
struct DivAt {
    col: usize,
    dy: i32,
    rows: Vec<DRow>,
}

/// A div's rows inside its background, padding and border: `cols` wide,
/// `rows` high (else as tall as the content needs).
fn boxed_div(content: Vec<DRow>, deco: &crate::console::DivBox, cols: usize, rows: Option<usize>) -> Vec<DRow> {
    let ins = deco.insets();
    let blank = |bg: Option<Rgb>| Cell {
        ch: ' ',
        w: 1,
        style: Style { fg: Rgb(0, 0, 0), bg, bold: false, italic: false, underline: false, strike: false },
        button: None,
    };
    let edge = blank(deco.bcolor);
    let fill = blank(deco.bg);
    let height = rows.unwrap_or(content.len() + ins[0] + ins[2]).max(ins[0] + ins[2] + 1);
    let mut content = content.into_iter();
    let mut out = Vec::with_capacity(height);
    for r in 0..height {
        let mut row = DRow::default();
        let border_row = (r == 0 && deco.border[0]) || (r + 1 == height && deco.border[2]);
        if border_row {
            row.cells = vec![edge.clone(); cols];
            out.push(row);
            continue;
        }
        if deco.border[3] {
            row.cells.push(edge.clone());
        }
        row.cells.extend(std::iter::repeat_n(fill.clone(), deco.pad[3]));
        let start = row.cells.len();
        let in_w = cols.saturating_sub(ins[1] + ins[3]);
        if r >= ins[0] && r + ins[2] < height {
            if let Some(c) = content.next() {
                for mut cell in c.cells {
                    if cell.style.bg.is_none() {
                        cell.style.bg = deco.bg;
                    }
                    row.cells.push(cell);
                }
                for mut im in c.imgs {
                    im.col += start;
                    row.imgs.push(im);
                }
                for mut d in c.divs {
                    d.col += start;
                    row.divs.push(d);
                }
            }
        }
        // pad (or cut) the content to the inner width
        let mut used = 0usize;
        let mut k = start;
        while k < row.cells.len() {
            let w = row.cells[k].w.max(1) as usize;
            if used + w > in_w {
                row.cells.truncate(k);
                break;
            }
            used += w;
            k += 1;
        }
        row.cells.extend(std::iter::repeat_n(fill.clone(), in_w.saturating_sub(used) + deco.pad[1]));
        if deco.border[1] {
            row.cells.push(edge.clone());
        }
        out.push(row);
    }
    // rounded corners: a quarter block in the corner cell's colour
    if height > 1 && cols > 1 {
        for (k, glyph) in ['▗', '▖', '▘', '▝'].into_iter().enumerate() {
            if !deco.round[k] {
                continue;
            }
            let row = if k < 2 { out.first_mut() } else { out.last_mut() };
            let cell = row.and_then(|r| if k == 0 || k == 3 { r.cells.first_mut() } else { r.cells.last_mut() });
            if let Some(c) = cell.filter(|c| c.ch == ' ' && c.style.bg.is_some()) {
                c.ch = glyph;
                c.style.fg = c.style.bg.take().unwrap();
            }
        }
    }
    out
}

/// Paint `cells` over `row` from screen column `col`: covered cells (and
/// wide characters cut in half) give way, the row is padded if short.
fn paint_cells(row: &mut DRow, col: usize, cells: Vec<Cell>) {
    if cells.is_empty() {
        return;
    }
    let width: usize = cells.iter().map(|c| c.w.max(1) as usize).sum();
    let end = col + width;
    let blank = |st: &Style| Cell { ch: ' ', w: 1, style: st.clone(), button: None };
    let plain = Style { fg: Rgb(0, 0, 0), bg: None, bold: false, italic: false, underline: false, strike: false };
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut c = 0usize;
    for cell in std::mem::take(&mut row.cells) {
        let w = cell.w.max(1) as usize;
        if c + w <= col {
            before.push(cell);
        } else if c >= end {
            after.push(cell);
        } else {
            // partly covered: keep the uncovered columns blank
            for k in c..c + w {
                if k < col {
                    before.push(blank(&cell.style));
                } else if k >= end {
                    after.push(blank(&cell.style));
                }
            }
        }
        c += w;
    }
    while c < col {
        before.push(blank(&plain));
        c += 1;
    }
    before.extend(cells);
    before.extend(after);
    row.cells = before;
}

/// A decoded input event. Mouse `btn` uses xterm codes: 0/1/2 buttons,
/// +32 motion, 64/65 wheel; `x`/`y` are 1-based cells.
#[derive(Debug)]
enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    CtrlC,
    CtrlL,
    Left,
    Right,
    /// F1 or Ctrl-O: the settings overlay
    Settings,
    /// Emuera-Anchor's hotkeys: Ctrl-T title, Ctrl-R restart, F5 / Ctrl-E
    /// reload the ERB files
    Title,
    Restart,
    Reload,
    /// Tab: continue and skip messages until a choice (as a right click)
    Skip,
    Mouse {
        btn: u16,
        x: u16,
        y: u16,
        press: bool,
    },
    /// Insert, or Shift+Enter where the terminal tells it from Enter: a
    /// click with the keyboard pointer
    Click,
    Other,
}

/// What the settings overlay asks the input loop to do next.
enum OverlayExit {
    Close,
    /// the game's settings (emuera.config) changed
    Config,
    Title,
    Restart,
    Reload,
    SwitchGame,
    Quit,
}

pub struct Tty {
    raw_mode: crate::sys::RawMode,
    gfx: Box<dyn Backend>,
    cell: (u32, u32),
    cols: usize,
    rows: usize,
    color: ColorMode,
    scroll: usize,
    buf: String,
    /// the last input timed out (TINPUT): what was typed stays for the
    /// next, as Emuera's input box keeps it (AnonTW's animated map is a
    /// TINPUT loop)
    keep_buf: bool,
    /// GETKEY: keys / mouse buttons held (Windows codes: 1 left, 2 right)
    keys_down: std::collections::HashSet<i64>,
    /// GETKEYTRIGGERED: pressed since asked last
    keys_hit: std::collections::HashSet<i64>,
    pending: Vec<u8>,
    /// keys decoded but not yet handed out (console mouse events)
    queued: std::collections::VecDeque<Key>,
    /// pointer on the Linux console via /dev/input/mice (no terminal reports)
    mice: Option<crate::mice::Mice>,
    mouse: Option<(usize, usize)>,
    /// How characters Emuera draws two columns wide but the terminal draws
    /// one column wide are handled (detected at startup, see `char_cells`).
    ambi: Ambi,
    /// what the Linux console's font can draw (None: any terminal font)
    font_chars: Option<std::collections::HashSet<char>>,
    /// Widths this terminal actually draws, for characters where tables
    /// disagree (measured at startup; only entries that differ from
    /// unicode-width are kept).
    widths: std::collections::HashMap<char, u8>,
    /// per screen row: (col_start, col_end, button id)
    btn_map: Vec<Vec<(usize, usize, usize)>>,
    buttons: Vec<Value>,
    hover: Option<usize>,
    active: bool,
    /// the terminal went away (hangup / end of input): quit instead of
    /// waiting forever on a dead stdin
    hangup: bool,
    /// a setup problem the user should see (e.g. no console mouse)
    notice: Option<String>,
    fb_cell_fixed: bool,
    /// the last frame, for moving the console pointer without a full
    /// redraw (which makes images flicker): per screen row (col, cell,
    /// hover colour), the images, the pointer cell and the input cursor
    frame: Vec<Vec<(usize, Cell, Option<Rgb>)>>,
    frame_imgs: Vec<Placement>,
    /// makes overlapping pictures into one (see `gfx::Merger`)
    merger: crate::gfx::Merger,
    /// the button the pointer rests on and since when (tooltip delay)
    tip_since: Option<(usize, Instant)>,
    /// the last frame showed a tooltip
    tip_drawn: bool,
    drawn_pointer: Option<(usize, usize)>,
    frame_cursor: Option<(usize, usize)>,
    /// the window background for this frame (None: the terminal's own)
    screen_bg: Option<Rgb>,
    /// minimum contrast for coloured text (1: off), see `console::readable`
    pub contrast: f32,
    /// colours drawn as others (`recolor` setting: AnonTW's time-stop navy
    /// as a softer blue…); the game still sees its own
    pub recolor: Vec<(Rgb, Rgb)>,
    /// lines per mouse-wheel step
    pub wheel: usize,
    /// show the hovered button's value, memory and the running function
    /// in the bottom row
    pub status_info: bool,
    /// the keyboard pointer (see `kbd_pointer`)
    pub kbd_ptr: bool,
    /// EMUERA_MOUSE_LOG: what the mouse did, for finding out why a pointer
    /// misbehaves where it can't be watched (the Linux console)
    mouse_log: Option<std::fs::File>,
    /// console mouse events received (shown in the bottom bar there)
    mouse_events: u64,
    /// our pointer blinks: shown now, and when that changes
    ptr_on: bool,
    blink_at: Instant,
    /// what applying a setting has to say (shown on the settings page)
    setting_note: String,
    /// live translation of character dialog (see `mt`): the translator, and
    /// into / from which language, and what (dialog, all)
    mt: Option<crate::mt::Mt>,
    tr_to: String,
    tr_from: String,
    tr_all: bool,
    /// the memory in use in the bottom row too (it is always in F1)
    pub show_memory: bool,
    /// the bottom row's right side as last drawn: (columns the prompt may
    /// use, the function asking, the column it starts at); None: not shown
    bar: Option<(usize, String, usize)>,
    /// memory in use, read at most once a second: (when, bytes)
    mem_seen: Option<(Instant, Option<u64>)>,
    /// when the script started its current stretch of computing
    busy_since: Option<Instant>,
    /// the "Copy to clipboard" setting: game (emuera.config), on, off
    pub clip_mode: String,
    clip: crate::clipboard::Clipboard,
    /// the last left click (time, cell), to tell double clicks
    last_click: Option<(Instant, (u16, u16))>,
    /// frames drawn without images (under the settings overlay)
    no_images: bool,
    /// the colour mode detected at startup, for the "auto" setting
    auto_color: ColorMode,
    /// the `--gfx` preference and $TERM, to set the image backend up again
    gfx_pref: String,
    term: String,
    /// fingerprint of the last frame sent (an identical one is skipped)
    last_frame: u64,
    force_frame: bool,
    /// the game's "ボタンの途中で行を折りかえさない" setting (per frame)
    button_wrap: bool,
    /// CBG pictures scaled to the terminal, by (fingerprint, size)
    cbg_scaled: std::collections::HashMap<(u64, u32, u32), std::rc::Rc<image::RgbaImage>>,
    /// the game folder and its save folder, for the save manager
    game: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// an animated sprite was on the last frame
    animating: bool,
    /// fbterm: 256 colours through its own escapes
    fbterm: bool,
    /// hash of each screen row as last written (0: unknown, write it)
    row_hash: Vec<u64>,
    hash_cols: usize,
    /// something drew over the rows outside `render` (overlay boxes)
    redraw_all: std::cell::Cell<bool>,
    /// an overlay box (list, prompt, page) closed or changed size: the
    /// next frame is drawn in full even if the game's screen is the same
    box_closed: std::cell::Cell<bool>,
    /// buttons with a `srcb` hover picture (their hover needs a full redraw)
    hover_imgs: std::collections::HashSet<usize>,
    /// translated lines the player clicked back to the original text
    /// (see [`line_key`])
    show_orig: std::collections::HashSet<u64>,
    /// per screen row of the last frame: the key of the translatable line
    /// it belongs to, for clicks
    row_lines: Vec<Option<u64>>,
}

/// Terminal size: (cols, rows, pixel width, pixel height); pixels are 0
/// when the terminal doesn't report them.
fn winsize() -> (usize, usize, u32, u32) {
    crate::sys::winsize().unwrap_or((80, 24, 0, 0))
}

/// Decode an X10 / urxvt button byte (already minus 32). Releases come as
/// button 3 without saying which button: report them as a left release.
fn x10_mouse(cb: u16, x: u16, y: u16) -> Key {
    let release = cb & 3 == 3 && cb & (32 | 64) == 0;
    Key::Mouse { btn: if release { cb & !3 } else { cb }, x, y, press: !release }
}

/// A character the Linux console's font has instead of `ch` (which it
/// would show as a diamond): the nearest ASCII or console-font lookalike.
fn console_lookalike(ch: char, has: &std::collections::HashSet<char>) -> char {
    let cands: &[char] = match ch {
        // full-width ASCII
        '\u{FF01}'..='\u{FF5E}' => return char::from_u32(ch as u32 - 0xFEE0).unwrap_or('?'),
        '\u{3000}' => return ' ',
        '【' | '〔' | '「' | '『' | '〈' | '《' | '〖' | '〘' | '〚' => &['[', '['],
        '】' | '〕' | '」' | '』' | '〉' | '》' | '〗' | '〙' | '〛' => &[']', ']'],
        '・' | '･' | '·' => &['·', '.'],
        '、' | '､' => &[','],
        '。' | '｡' => &['.'],
        '…' | '‥' => &['…', '.'],
        'ー' | '―' | '‐' | '—' | '–' | '─' | '━' => &['─', '-'],
        '〜' => &['~'],
        '“' | '”' | '″' | '〝' | '〟' => &['"'],
        '‘' | '’' | '′' => &['\''],
        '★' | '☆' | '※' => &['*'],
        '○' | '◯' | '◎' | '〇' => &['○', 'o'],
        '●' => &['●', '•', 'o'],
        '□' | '◇' => &['□', '#'],
        '■' | '◆' => &['■', '#'],
        '△' | '▲' => &['▲', '^'],
        '▽' | '▼' => &['▼', 'v'],
        '→' | '⇒' => &['→', '>'],
        '←' | '⇐' => &['←', '<'],
        '↑' => &['↑', '^'],
        '↓' => &['↓', 'v'],
        '×' => &['×', 'x'],
        '÷' => &['÷', '/'],
        '±' => &['±', '+'],
        '♪' | '♫' => &['♪', '#'],
        '♡' | '♥' => &['♥', '<'],
        '≠' => &['≠', '#'],
        '≦' | '≤' => &['≤', '<'],
        '≧' | '≥' => &['≥', '>'],
        '┃' | '│' => &['│', '|'],
        // nothing like it (kana, kanji): the console's diamond says so
        _ => return ch,
    };
    cands.iter().copied().find(|c| c.is_ascii() || has.contains(c)).unwrap_or(ch)
}

/// How to draw one character so the screen matches Emuera's layout:
/// (cells the terminal draws it in, filler cells to add, filler char).
///
/// Emuera games are laid out for a Japanese font where every Shift_JIS
/// double-byte character is two columns wide, including box drawing
/// (`━┃┏`), blocks (`■□`) and symbols (`※○→`). Most terminals draw those
/// "East Asian ambiguous" characters one column wide, which skews ASCII
/// maps. The missing column is filled: line and block characters continue
/// themselves (`━━`, `┏━`, `■■`) so walls stay solid; others get a space.
/// With [`Ambi::Osc66`] such a character is instead drawn as one 2-cell
/// glyph (no filler); with [`Ambi::Wide`] the terminal already draws it wide.
/// `measured`: the width this terminal was seen to draw `ch` at (see
/// `Tty::measure_widths`), which beats any table.
fn char_cells(ch: char, ambi: Ambi, measured: Option<u8>) -> (usize, usize, char) {
    let tw = match measured {
        Some(w) => w as usize,
        None => if ambi == Ambi::Wide { UnicodeWidthChar::width_cjk(ch) } else { UnicodeWidthChar::width(ch) }.unwrap_or(0),
    };
    // a terminal with no double-width cells (the Linux console before
    // 6.15) draws every character in one cell
    let tw = if ambi == Ambi::Narrow { tw.min(1) } else { tw };
    if tw == 0 || ch.is_ascii() {
        return (tw, 0, ' ');
    }
    let mut buf = [0u8; 4];
    let (bytes, _, unmappable) = encoding_rs::SHIFT_JIS.encode(ch.encode_utf8(&mut buf));
    let emuera = if unmappable { tw } else { bytes.len() };
    let fill = match ch {
        '━' | '┏' | '┗' | '┣' | '┳' | '┻' | '╋' | '┯' | '┷' | '┿' | '┝' | '┍' | '┕' => '━',
        '─' | '┌' | '└' | '├' | '┬' | '┴' | '┼' | '┠' | '┰' | '┸' | '╂' | '┎' | '┖' => '─',
        '═' | '╔' | '╚' | '╠' | '╦' | '╩' | '╬' => '═',
        '┄' | '┅' | '┈' | '┉' | '╌' | '╍' | '■' | '█' | '▓' | '▒' | '░' | '―' | '‐' | '〓' | '≡' | 'Ⅱ' | 'Ⅲ' => ch,
        _ => ' ',
    };
    let extra = emuera.saturating_sub(tw);
    if ambi == Ambi::Osc66 && extra > 0 {
        return (tw + extra, 0, ' ');
    }
    (tw, extra, fill)
}

/// A tooltip laid out: its lines (text, columns) and its box (1-based
/// top row and left column, width in columns).
struct TipBox {
    lines: Vec<(String, usize)>,
    top: usize,
    left: usize,
    w: usize,
}

/// Text from the game safe to write into a box: a tab becomes a space,
/// other control characters go (CR and LF stay for the caller to split
/// lines at); written raw they would move the cursor.
pub(crate) fn printable(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\r' | '\n' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Strategy for East Asian "ambiguous width" characters (`━ ■ ※ …`).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ambi {
    /// draw one cell, then a filler cell (works everywhere)
    Pad,
    /// the terminal already draws them two cells wide
    Wide,
    /// the terminal draws even CJK in one cell (Linux console): every wide
    /// character is padded to its Emuera width
    Narrow,
    /// kitty's text sizing protocol: `OSC 66 ; w=2 ; ch` draws the glyph
    /// across two cells (opt-in: kitty's rendering of it doesn't match
    /// Emuera's font well)
    Osc66,
}

/// `from:to` pairs of hex colours, comma separated (`01183c:111d3b`,
/// `#` or `0x` allowed); pairs that don't parse are left out.
fn parse_recolor(v: &str) -> Vec<(Rgb, Rgb)> {
    let hex = |s: &str| {
        let s = s.trim().trim_start_matches('#');
        let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
        u32::from_str_radix(s, 16).ok().filter(|_| !s.is_empty()).map(|n| Rgb::from_i64(n as i64))
    };
    v.split(',').filter_map(|p| p.split_once(':')).filter_map(|(a, b)| Some((hex(a)?, hex(b)?))).collect()
}

/// Write to the terminal and flush.
fn write_all(b: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b);
    let _ = out.flush();
}

/// Running inside tmux?
fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Does tmux let escape sequences through to the terminal it runs in
/// (`allow-passthrough on` or `all`)? Without it, pictures can't be shown.
fn tmux_passthrough() -> bool {
    std::process::Command::new("tmux")
        .args(["show-options", "-gqv", "allow-passthrough"])
        .output()
        .is_ok_and(|o| matches!(String::from_utf8_lossy(&o.stdout).trim(), "on" | "all"))
}

impl Tty {
    /// Enter raw mode and the alternate screen, pick colour and graphics
    /// support, open the console mouse if needed, and enable mouse reports.
    pub fn new(gfx_pref: &str) -> Result<Tty, String> {
        let raw_mode = crate::sys::RawMode::enter()?;
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        let forced = match std::env::var("EMUERA_COLORS").as_deref() {
            Ok("truecolor") | Ok("24bit") => Some(ColorMode::True),
            Ok("256") => Some(ColorMode::C256),
            Ok("16") => Some(ColorMode::C16),
            _ => None,
        };
        let detected = if term == "linux" {
            ColorMode::C16
        } else if term.starts_with("fbterm") {
            // with its own escapes (see `sgr`)
            ColorMode::C256
        } else if colorterm.contains("truecolor") || colorterm.contains("24bit") || term.contains("kitty") || term.contains("foot") || term.contains("direct") {
            ColorMode::True
        } else if term.contains("256") {
            ColorMode::C256
        } else {
            ColorMode::True
        };
        let color = forced.unwrap_or(detected);
        let (cols, rows, xp, yp) = winsize();
        let mut t = Tty {
            raw_mode,
            gfx: Box::new(gfx::NoGfx),
            cell: (8, 16),
            cols,
            rows,
            color,
            scroll: 0,
            buf: String::new(),
            keep_buf: false,
            keys_down: Default::default(),
            keys_hit: Default::default(),
            pending: Vec::new(),
            queued: std::collections::VecDeque::new(),
            mice: None,
            mouse: None,
            ambi: Ambi::Pad,
            font_chars: None,
            widths: std::collections::HashMap::new(),
            btn_map: Vec::new(),
            buttons: Vec::new(),
            hover: None,
            active: true,
            hangup: false,
            notice: None,
            fb_cell_fixed: false,
            frame: Vec::new(),
            frame_imgs: Vec::new(),
            merger: Default::default(),
            tip_since: None,
            tip_drawn: false,
            drawn_pointer: None,
            frame_cursor: None,
            screen_bg: None,
            contrast: 3.0,
            recolor: Vec::new(),
            wheel: 3,
            status_info: true,
            kbd_ptr: false,
            mouse_log: std::env::var_os("EMUERA_MOUSE_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()),
            mouse_events: 0,
            ptr_on: true,
            blink_at: Instant::now() + Duration::from_millis(530),
            setting_note: String::new(),
            mt: None,
            tr_to: String::new(),
            tr_from: "auto".into(),
            tr_all: false,
            show_memory: false,
            bar: None,
            mem_seen: None,
            busy_since: None,
            clip_mode: "game".into(),
            clip: crate::clipboard::Clipboard::default(),
            last_click: None,
            no_images: false,
            auto_color: detected,
            gfx_pref: gfx_pref.to_string(),
            term: term.clone(),
            last_frame: 0,
            force_frame: false,
            cbg_scaled: std::collections::HashMap::new(),
            game: None,
            animating: false,
            fbterm: term.starts_with("fbterm"),
            row_hash: Vec::new(),
            hash_cols: 0,
            redraw_all: std::cell::Cell::new(true),
            box_closed: std::cell::Cell::new(false),
            hover_imgs: std::collections::HashSet::new(),
            show_orig: std::collections::HashSet::new(),
            row_lines: Vec::new(),
            button_wrap: false,
        };
        if xp > 0 && yp > 0 {
            t.cell = (xp / cols as u32, yp / rows as u32);
        }
        let gfx_note = t.setup_gfx(gfx_pref, &term);
        eprintln!("{}", gfx_note);
        t.ambi = t.detect_ambiguous();
        // (every width is 1 there: nothing to measure)
        if t.ambi != Ambi::Narrow {
            t.widths = t.measure_widths();
        }
        let is_vt = term == "linux" || term.starts_with("fbterm");
        // (fbterm draws with fontconfig's fonts)
        if term == "linux" {
            t.font_chars = crate::sys::console_font_chars();
        }
        if crate::mice::Mice::wanted(is_vt) {
            // gpm first (works without extra permissions), else the device
            let gpm = if std::env::var("EMUERA_MOUSE").as_deref() == Ok("mice") { Err(String::new()) } else { crate::mice::Mice::open_gpm() };
            match gpm.or_else(|_| crate::mice::Mice::open()) {
                Ok(m) => {
                    t.mice = Some(m);
                    // the pointer shows from the start, mid-screen, until
                    // the mouse moves it
                    t.mouse = Some((t.cols / 2, t.rows / 2));
                }
                Err(e) => {
                    // shown on the input line (and again on exit): the
                    // startup text is gone once the screen is drawn
                    t.notice = Some(format!("no mouse ({}): install gpm, or add yourself to the input group", e));
                }
            }
        }
        // alt screen, hide cursor; mouse reports: buttons, drag, any motion,
        // in SGR (1006) or urxvt (1015) form where supported, else X10
        write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J");
        // the terminal's mouse reports — but not with a console mouse of our
        // own: on the Linux console they make gpm send the mouse to the
        // console (clicks only, as X10 reports) instead of to us, and the
        // pointer never moved
        if t.mice.is_none() {
            write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h");
        }
        // xterm's modifyOtherKeys (Shift+Enter apart from Enter); not for
        // the Linux console, which has no such mode
        if !is_vt {
            write_all(b"\x1b[>4;1m");
        }
        t.mlog(&format!(
            "TERM={} gfx={} cell={:?} size={}x{} mouse={}",
            term,
            t.gfx.name(),
            t.cell,
            t.cols,
            t.rows,
            t.mice.as_ref().map_or_else(|| t.notice.clone().unwrap_or_else(|| "none".into()), |m| m.describe().to_string())
        ));
        Ok(t)
    }

    /// Wait up to `timeout_ms` for input on stdin (and the console mouse);
    /// returns whether anything arrived.
    fn read_bytes(&mut self, timeout_ms: i32) -> bool {
        let r = crate::sys::poll(timeout_ms, self.mice.as_ref().map_or(-1, |m| m.fd()));
        if r.hangup {
            self.hangup = true;
            return false;
        }
        if r.extra {
            let (cell, cols, rows) = (self.cell, self.cols, self.rows);
            let evs = self.mice.as_mut().map(|m| m.read(cell, cols, rows)).unwrap_or_default();
            for (btn, x, y, press) in evs {
                self.mouse_events += 1;
                self.mlog(&format!("event btn={} cell=({},{}) press={}", btn, x, y, press));
                self.queued.push_back(Key::Mouse { btn, x, y, press });
            }
        }
        if !r.input {
            return !self.queued.is_empty();
        }
        let mut b = [0u8; 4096];
        match crate::sys::read_stdin(&mut b) {
            Ok(n) if n > 0 => {
                self.pending.extend_from_slice(&b[..n]);
                true
            }
            // an error other than "try again"
            Err(e) if matches!(e.kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock) => false,
            // 0: end of input
            _ => {
                self.hangup = true;
                false
            }
        }
    }

    /// Is unread input waiting on stdin right now?
    fn input_waiting(&self) -> bool {
        crate::sys::poll(0, -1).input
    }

    /// Send a query and collect the reply until `done` or timeout.
    fn query(&mut self, q: &[u8], done: impl Fn(&[u8]) -> bool, ms: u64) -> Vec<u8> {
        self.pending.clear();
        write_all(q);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(ms) {
            self.read_bytes(20);
            if done(&self.pending) {
                break;
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Ask the terminal how wide it draws each character that is two columns
    /// in Emuera but whose width terminals disagree on (Shift_JIS
    /// double-byte symbols, box drawing, Greek/Cyrillic, kana; ~800). All
    /// probes go out in one write — `\r ch CSI 6n` each — and the replies'
    /// cursor columns give the widths. Terminals that don't answer leave the
    /// table empty. `EMUERA_MEASURE=0` skips this.
    fn measure_widths(&mut self) -> std::collections::HashMap<char, u8> {
        let mut out = std::collections::HashMap::new();
        if std::env::var("EMUERA_MEASURE").as_deref() == Ok("0") {
            return out;
        }
        let chars: Vec<char> = (0xA0u32..0x3400)
            .chain(0xFF00..0xFFF0)
            .filter_map(char::from_u32)
            .filter(|c| {
                let mut b = [0u8; 4];
                let (bytes, _, bad) = encoding_rs::SHIFT_JIS.encode(c.encode_utf8(&mut b));
                !bad && bytes.len() == 2
            })
            .collect();
        let mut q = String::with_capacity(chars.len() * 8);
        for c in &chars {
            q.push('\r');
            q.push(*c);
            q.push_str("\x1b[6n");
        }
        q.push_str("\r\x1b[K");
        self.pending.clear();
        write_all(q.as_bytes());
        // collect replies until all arrived or the terminal goes quiet
        let count = |b: &[u8]| b.iter().filter(|&&x| x == b'R').count();
        let start = Instant::now();
        while count(&self.pending) < chars.len() && start.elapsed() < Duration::from_millis(1500) {
            if !self.read_bytes(150) && count(&self.pending) == 0 && start.elapsed() > Duration::from_millis(400) {
                break; // no replies at all: terminal doesn't answer
            }
        }
        let text = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).to_string();
        let cols: Vec<u32> = text.split('R').filter_map(|r| r.rfind("\x1b[").and_then(|i| r[i + 2..].split(';').nth(1)).and_then(|c| c.parse().ok())).collect();
        if cols.len() == chars.len() {
            for (c, col) in chars.iter().zip(cols) {
                let w = col.saturating_sub(1).min(2) as u8;
                // 0 = drew nothing / unknown: keep the table's answer
                if w > 0 && UnicodeWidthChar::width(*c) != Some(w as usize) {
                    out.insert(*c, w);
                }
            }
        }
        out
    }

    /// Pick the [`Ambi`] strategy: print `■` and ask where the cursor went
    /// (CSI 6n). Column 3 means the terminal already draws it wide; else pad.
    /// `EMUERA_AMBIGUOUS=wide|narrow|osc66` overrides. OSC 66 is opt-in only:
    /// kitty draws box drawing stretched (lines look doubled) and blocks
    /// small with a gap, so padding looks closer to Emuera.
    fn detect_ambiguous(&mut self) -> Ambi {
        match std::env::var("EMUERA_AMBIGUOUS").as_deref() {
            Ok("wide") => return Ambi::Wide,
            Ok("narrow") => return Ambi::Pad,
            Ok("osc66") => return Ambi::Osc66,
            Ok("single") => return Ambi::Narrow,
            _ => {}
        }
        let cursor_col = |t: &mut Tty, probe: &str| -> Option<u32> {
            let r = t.query(format!("\r\x1b[K{}\x1b[6n", probe).as_bytes(), |b| b.contains(&b'R'), 300);
            write_all(b"\r\x1b[K");
            let s = String::from_utf8_lossy(&r).to_string();
            s.rfind("\x1b[").and_then(|i| s[i + 2..].trim_end_matches('R').split(';').nth(1).and_then(|c| c.parse::<u32>().ok()))
        };
        // a kanji that moves the cursor one cell: no double-width at all
        if cursor_col(self, "漢") == Some(2) {
            return Ambi::Narrow;
        }
        if cursor_col(self, "■") == Some(3) {
            Ambi::Wide
        } else {
            Ambi::Pad
        }
    }

    /// Choose the image backend: framebuffer on a VT, otherwise probe for
    /// kitty graphics, then sixel (DA1 attribute 4); also learn the cell
    /// size in pixels so images can be scaled. Returns what was chosen (and
    /// why the framebuffer wasn't), for stderr at start or the settings
    /// page later: printed while the game is on screen it would land on it.
    fn setup_gfx(&mut self, pref: &str, term: &str) -> String {
        let is_vt = term == "linux" || term.starts_with("fbterm");
        let mut why = String::new();
        let mut try_fb = |t: &mut Tty| -> bool {
            match gfx::fb::Fb::open() {
                Ok(fb) => {
                    t.cell = ((fb.xres / t.cols as u32).max(1), (fb.yres / t.rows as u32).max(1));
                    t.fb_cell_fixed = true;
                    t.gfx = Box::new(fb);
                    true
                }
                Err(e) => {
                    let hint = if e.to_string().contains("ermission") { " (/dev/fb0 needs the video group)" } else { "" };
                    why = format!("; framebuffer unavailable: {}{}", e, hint);
                    false
                }
            }
        };
        match pref {
            "none" => return "graphics backend: none".into(),
            "kitty" if in_tmux() => self.gfx = Box::new(gfx::kitty::Kitty::new_tmux()),
            "kitty" => self.gfx = Box::new(gfx::kitty::Kitty::new()),
            "iterm" => self.gfx = Box::new(gfx::iterm::Iterm::new()),
            "sixel" => self.gfx = Box::new(gfx::sixel::Sixel::new()),
            "fb" => {
                try_fb(self);
            }
            _ => {
                // iTerm2 says who it is (and has no kitty graphics)
                let iterm = std::env::var("TERM_PROGRAM").as_deref() == Ok("iTerm.app") || std::env::var("LC_TERMINAL").as_deref() == Ok("iTerm2");
                if is_vt {
                    try_fb(self);
                } else if iterm {
                    self.gfx = Box::new(gfx::iterm::Iterm::new());
                } else {
                    // inside tmux the kitty query goes through its
                    // passthrough (the terminal's answer may not come back:
                    // the terminal tmux runs in also says who it is through
                    // the environment); tmux answers the attributes itself
                    // (sixel if it was built with it, and it then shows
                    // sixel pictures on its own)
                    let tmux = in_tmux();
                    let passthrough = tmux && tmux_passthrough();
                    if std::env::var_os("STY").is_some() && !tmux {
                        why = "; inside GNU screen there are no pictures (tmux can show them)".into();
                    }
                    if tmux && !passthrough {
                        why = "; inside tmux, kitty-graphics terminals (Ghostty, kitty) need `set -g allow-passthrough on` for pictures".into();
                    }
                    let kitty_q: &[u8] = if passthrough {
                        b"\x1bPtmux;\x1b\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\x1b\\\x1b\\\x1b[c"
                    } else {
                        b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c"
                    };
                    let r = self.query(
                        kitty_q,
                        |b| {
                            let s = String::from_utf8_lossy(b);
                            s.contains("\x1b[?") && s.trim_end().ends_with('c')
                        },
                        400,
                    );
                    let s = String::from_utf8_lossy(&r).to_string();
                    // (terminals with kitty's Unicode placeholders)
                    let placeholders = std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() || std::env::var_os("KITTY_WINDOW_ID").is_some();
                    if passthrough && (s.contains("_Gi=31;OK") || placeholders) {
                        self.gfx = Box::new(gfx::kitty::Kitty::new_tmux());
                    } else if s.contains("_Gi=31;OK") && !tmux {
                        self.gfx = Box::new(gfx::kitty::Kitty::new());
                    } else if let Some(i) = s.find("\x1b[?") {
                        let da = &s[i + 3..];
                        let da = &da[..da.find('c').unwrap_or(da.len())];
                        if da.split(';').any(|x| x == "4") {
                            self.gfx = Box::new(gfx::sixel::Sixel::new());
                        }
                    }
                }
            }
        }
        if self.gfx.name() != "fb" && self.gfx.name() != "none" {
            let (_, _, xp, yp) = winsize();
            if xp == 0 || yp == 0 {
                // CSI 16 t -> CSI 6 ; h ; w t
                let r = self.query(b"\x1b[16t", |b| b.ends_with(b"t"), 300);
                let s = String::from_utf8_lossy(&r).to_string();
                if let Some(i) = s.find("\x1b[6;") {
                    let p: Vec<u32> = s[i + 4..].trim_end_matches('t').split(';').filter_map(|x| x.parse().ok()).collect();
                    if p.len() == 2 && p[0] > 0 && p[1] > 0 {
                        self.cell = (p[1], p[0]);
                    }
                }
            }
        }
        format!("graphics backend: {} (cell {}x{} px){}", self.gfx.name(), self.cell.0, self.cell.1, why)
    }

    /// Decode the next key or mouse event, waiting up to `timeout_ms`.
    /// Incomplete escape sequences wait briefly for their remaining bytes.
    fn next_key(&mut self, timeout_ms: i32) -> Option<Key> {
        if let Some(k) = self.queued.pop_front() {
            return Some(k);
        }
        if self.pending.is_empty() && !self.read_bytes(timeout_ms) {
            return None;
        }
        if let Some(k) = self.queued.pop_front() {
            return Some(k);
        }
        if self.pending.is_empty() {
            return None;
        }
        let b = &self.pending;
        let (k, n) = match b[0] {
            0x1b => {
                if b.len() == 1 {
                    // lone ESC? wait briefly for the rest of a sequence
                    if !self.read_bytes(30) {
                        self.pending.clear();
                        return Some(Key::Esc);
                    }
                    return self.next_key(0);
                }
                let b = self.pending.clone();
                if b[1] == b'[' {
                    if b.get(2) == Some(&b'[') {
                        // the Linux console's F1–F5: ESC [ [ A … E (the second
                        // [ is no final byte here)
                        if b.len() < 4 {
                            self.read_bytes(30);
                            if self.pending.len() < 4 {
                                self.pending.clear();
                                return Some(Key::Other);
                            }
                            return self.next_key(0);
                        }
                        (
                            match b[3] {
                                b'A' => Key::Settings,
                                b'E' => Key::Reload,
                                _ => Key::Other,
                            },
                            4,
                        )
                    } else if b.get(2) == Some(&b'M') {
                        // X10 / normal mouse: ESC [ M Cb Cx Cy, each value + 32
                        if b.len() < 6 {
                            self.read_bytes(30);
                            if self.pending.len() < 6 {
                                self.pending.clear();
                                return Some(Key::Other);
                            }
                            return self.next_key(0);
                        }
                        let (cb, cx, cy) = (b[3].wrapping_sub(32) as u16, b[4].wrapping_sub(32) as u16, b[5].wrapping_sub(32) as u16);
                        (x10_mouse(cb, cx, cy), 6)
                    } else if b.get(2) == Some(&b'<') {
                        // SGR mouse: ESC [ < b ; x ; y (M|m)
                        match b.iter().position(|&c| c == b'M' || c == b'm') {
                            Some(end) => {
                                let s = String::from_utf8_lossy(&b[3..end]).to_string();
                                let p: Vec<u16> = s.split(';').filter_map(|x| x.parse().ok()).collect();
                                let k = if p.len() == 3 { Key::Mouse { btn: p[0], x: p[1], y: p[2], press: b[end] == b'M' } } else { Key::Other };
                                (k, end + 1)
                            }
                            None => {
                                self.read_bytes(30);
                                return self.next_key(0);
                            }
                        }
                    } else {
                        let end = b[2..].iter().position(|c| (0x40..=0x7e).contains(c)).map(|i| i + 2);
                        match end {
                            Some(e) => {
                                let k = match (&b[2..e], b[e]) {
                                    // urxvt 1015 mouse: ESC [ Cb ; x ; y M  (Cb + 32, decimal)
                                    (p, b'M') if p.iter().filter(|&&c| c == b';').count() == 2 => {
                                        let v: Vec<u16> = String::from_utf8_lossy(p).split(';').filter_map(|x| x.parse().ok()).collect();
                                        if v.len() == 3 {
                                            x10_mouse(v[0].saturating_sub(32), v[1], v[2])
                                        } else {
                                            Key::Other
                                        }
                                    }
                                    // F1: xterm-style, and the Linux console's ESC [ [ A
                                    (p, b'~') if p == b"11" => Key::Settings,
                                    (p, b'~') if p == b"15" => Key::Reload,
                                    (_, b'A') => Key::Up,
                                    (_, b'B') => Key::Down,
                                    (_, b'C') => Key::Right,
                                    (_, b'D') => Key::Left,
                                    (_, b'H') => Key::Home,
                                    (_, b'F') => Key::End,
                                    // Insert; Shift+Enter as xterm's modifyOtherKeys and
                                    // CSI-u terminals send it
                                    (p, b'~') if p == b"2" || p == b"27;2;13" => Key::Click,
                                    (p, b'u') if p == b"13;2" => Key::Click,
                                    (p, b'~') if p == b"5" => Key::PageUp,
                                    (p, b'~') if p == b"6" => Key::PageDown,
                                    (p, b'~') if p == b"1" || p == b"7" => Key::Home,
                                    (p, b'~') if p == b"4" || p == b"8" => Key::End,
                                    _ => Key::Other,
                                };
                                (k, e + 1)
                            }
                            None => (Key::Other, b.len()),
                        }
                    }
                } else if b[1] == b'O' && b.len() >= 3 {
                    (
                        match b[2] {
                            b'P' => Key::Settings,
                            b'A' => Key::Up,
                            b'B' => Key::Down,
                            b'C' => Key::Right,
                            b'D' => Key::Left,
                            // Konsole's Shift+Enter (and the keypad's Enter)
                            b'M' => Key::Click,
                            _ => Key::Other,
                        },
                        3,
                    )
                } else {
                    (Key::Esc, 1)
                }
            }
            b'\r' | b'\n' => (Key::Enter, 1),
            0x7f | 0x08 => (Key::Backspace, 1),
            0x03 => (Key::CtrlC, 1),
            0x0c => (Key::CtrlL, 1),
            0x0f => (Key::Settings, 1),
            0x14 => (Key::Title, 1),
            0x12 => (Key::Restart, 1),
            0x05 => (Key::Reload, 1),
            0x09 => (Key::Skip, 1),
            // Ctrl-U: clear the line being typed
            0x15 => (Key::Char('\u{15}'), 1),
            c if c < 0x20 => (Key::Other, 1),
            c => {
                let len = if c >= 0xf0 {
                    4
                } else if c >= 0xe0 {
                    3
                } else if c >= 0xc0 {
                    2
                } else {
                    1
                };
                if b.len() < len {
                    self.read_bytes(30);
                    if self.pending.len() < len {
                        self.pending.clear();
                        return Some(Key::Other);
                    }
                }
                let s = String::from_utf8_lossy(&self.pending[..len]).to_string();
                (Key::Char(s.chars().next().unwrap_or('?')), len)
            }
        };
        let n = n.min(self.pending.len());
        self.pending.drain(..n);
        Some(k)
    }

    // ------------------------------------------------------------ layout
    /// Wrap one console line into screen rows at the width it was printed
    /// with (`Line::cols`) and apply its alignment. Rows wider than the
    /// current screen are clipped by `render`. Buttons get ids (indices into
    /// `self.buttons`) only if the line belongs to the current input generation.
    fn layout_line(&mut self, line: &Line, cur_gen: u64, scale: f32) -> Vec<DRow> {
        let screen = self.cols.max(1);
        self.layout_line_in(line, cur_gen, scale, screen)
    }

    /// Lay out a line; `align_w` is the width center / right alignment
    /// works in (the screen, or a `<div>`).
    fn layout_line_in(&mut self, line: &Line, cur_gen: u64, scale: f32, align_w: usize) -> Vec<DRow> {
        let width = if line.cols > 0 { line.cols } else { self.cols.max(1) };
        let mut rows = vec![DRow::default()];
        let cw = self.cell.0.max(1) as f32;
        let mut col = 0usize;
        let active = line.gen == cur_gen;
        let push_btn = |v: &Option<Value>, buttons: &mut Vec<Value>| -> Option<usize> {
            match v {
                Some(v) if active => {
                    buttons.push(v.clone());
                    Some(buttons.len() - 1)
                }
                _ => None,
            }
        };
        for (si, seg) in line.segs.iter().enumerate() {
            match seg {
                Seg::Text { text, style, button, .. } => {
                    // Emuera's "ボタンの途中で行を折りかえさない": a button that
                    // doesn't fit the rest of the row starts the next one
                    // whole (its pieces in other colours count with it)
                    let starts_button =
                        button.is_some() && !matches!(si.checked_sub(1).and_then(|p| line.segs.get(p)), Some(Seg::Text { button: b, .. }) if b == button);
                    if self.button_wrap && starts_button && col > 0 {
                        let mut w = 0;
                        for s2 in &line.segs[si..] {
                            match s2 {
                                Seg::Text { text: t2, button: b2, .. } if b2 == button => {
                                    w += t2
                                        .chars()
                                        .map(|c| {
                                            let (tw, extra, _) = if c == '\t' { (4, 0, ' ') } else { char_cells(c, self.ambi, self.widths.get(&c).copied()) };
                                            tw + extra
                                        })
                                        .sum::<usize>();
                                }
                                _ => break,
                            }
                        }
                        if col + w > width && w <= width {
                            rows.push(DRow::default());
                            col = 0;
                        }
                    }
                    let bid = push_btn(button, &mut self.buttons);
                    for ch in text.chars() {
                        let (tw, extra, fill) = if ch == '\t' { (4, 0, ' ') } else { char_cells(ch, self.ambi, self.widths.get(&ch).copied()) };
                        let (ch, tw, extra, fill) = self.drawable(ch, tw, extra, fill);
                        let w = tw + extra;
                        if tw == 0 {
                            continue;
                        }
                        if col + w > width {
                            rows.push(DRow::default());
                            col = 0;
                        }
                        let r = rows.last_mut().unwrap();
                        if ch == '\t' {
                            for _ in 0..4 {
                                r.cells.push(Cell { ch: ' ', w: 1, style: style.clone(), button: bid });
                            }
                        } else {
                            r.cells.push(Cell { ch, w: tw as u8, style: style.clone(), button: bid });
                            for _ in 0..extra {
                                r.cells.push(Cell { ch: fill, w: 1, style: style.clone(), button: bid });
                            }
                        }
                        col += w;
                    }
                }
                Seg::Image { name, w, h, ypos, button, hover } => {
                    let bid = push_btn(button, &mut self.buttons);
                    if let (Some(b), Some(_)) = (bid, hover) {
                        self.hover_imgs.insert(b);
                    }
                    let cells = ((w * scale) / cw).round().max(1.0) as usize;
                    if col + cells > width && col > 0 {
                        rows.push(DRow::default());
                        col = 0;
                    }
                    let r = rows.last_mut().unwrap();
                    r.imgs.push(ImgAt { name: name.clone(), button: bid, hover: hover.clone(), col, w: *w, h: *h, ypos: *ypos });
                    let st = Style { fg: Rgb(90, 90, 90), bg: None, bold: false, italic: false, underline: false, strike: false };
                    let fill = if self.gfx.name() == "none" { '░' } else { ' ' };
                    for _ in 0..cells.min(width) {
                        r.cells.push(Cell { ch: fill, w: 1, style: st.clone(), button: bid });
                    }
                    col += cells.min(width);
                }
                Seg::Space { w } => {
                    let cells = ((w * scale) / cw).round().max(0.0) as usize;
                    let r = rows.last_mut().unwrap();
                    let st = Style { fg: Rgb(0, 0, 0), bg: None, bold: false, italic: false, underline: false, strike: false };
                    for _ in 0..cells.min(width.saturating_sub(col)) {
                        r.cells.push(Cell { ch: ' ', w: 1, style: st.clone(), button: None });
                    }
                    col = (col + cells).min(width);
                }
                Seg::Rect { w, color, button, .. } => {
                    let bid = push_btn(button, &mut self.buttons);
                    let cells = ((w * scale) / cw).round().max(1.0) as usize;
                    if col + cells > width && col > 0 {
                        rows.push(DRow::default());
                        col = 0;
                    }
                    let r = rows.last_mut().unwrap();
                    let st = Style { fg: *color, bg: None, bold: false, italic: false, underline: false, strike: false };
                    for _ in 0..cells.min(width) {
                        r.cells.push(Cell { ch: '█', w: 1, style: st.clone(), button: bid });
                    }
                    col += cells.min(width);
                }
                Seg::Div { dx, dy, cols, rows: max_rows, lines, deco, .. } => {
                    let ins = deco.insets();
                    let in_w = cols.saturating_sub(ins[1] + ins[3]).max(1);
                    let mut inner = Vec::new();
                    for l in lines {
                        inner.extend(self.layout_line_in(l, cur_gen, scale, in_w));
                    }
                    if let Some(m) = max_rows {
                        inner.truncate(m.saturating_sub(ins[0] + ins[2]).max(1));
                    }
                    if deco.bg.is_some() || deco.border.iter().any(|b| *b) || ins.iter().any(|n| *n > 0) {
                        inner = boxed_div(inner, deco, *cols, *max_rows);
                    }
                    let at = (col as i64 + *dx as i64).max(0) as usize;
                    rows.last_mut().unwrap().divs.push(DivAt { col: at, dy: *dy, rows: inner });
                }
            }
        }
        // alignment, within the screen (or div) as it is now
        if line.align != Align::Left {
            let screen = align_w.max(1);
            for r in rows.iter_mut() {
                let used: usize = r.cells.iter().map(|c| c.w as usize).sum();
                let pad = match line.align {
                    Align::Center => (screen.saturating_sub(used)) / 2,
                    _ => screen.saturating_sub(used),
                };
                if pad > 0 {
                    let st = Style { fg: Rgb(0, 0, 0), bg: None, bold: false, italic: false, underline: false, strike: false };
                    let mut nc: Vec<Cell> = (0..pad).map(|_| Cell { ch: ' ', w: 1, style: st.clone(), button: None }).collect();
                    nc.append(&mut r.cells);
                    r.cells = nc;
                    for im in r.imgs.iter_mut() {
                        im.col += pad;
                    }
                    for d in r.divs.iter_mut() {
                        d.col += pad;
                    }
                }
            }
        }
        rows
    }

    /// Emit the SGR sequence for a style in the terminal's colour mode.
    /// The background is the text's own, else the window's (SETBGCOLOR);
    /// the foreground is made readable against it (see `contrast`).
    fn sgr(&self, out: &mut Vec<u8>, st: &Style, fg_override: Option<Rgb>) {
        let bg = st.bg.map(|c| self.recolored(c)).or(self.screen_bg);
        let fg = crate::console::readable(self.recolored(fg_override.unwrap_or(st.fg)), bg.unwrap_or(Rgb(0, 0, 0)), self.contrast);
        out.extend_from_slice(b"\x1b[0");
        if st.bold {
            out.extend_from_slice(b";1");
        }
        if st.italic {
            out.extend_from_slice(b";3");
        }
        if st.underline {
            out.extend_from_slice(b";4");
        }
        if st.strike {
            out.extend_from_slice(b";9");
        }
        match self.color {
            ColorMode::True => {
                let _ = write!(out, ";38;2;{};{};{}", fg.0, fg.1, fg.2);
                if let Some(b) = bg {
                    let _ = write!(out, ";48;2;{};{};{}", b.0, b.1, b.2);
                }
            }
            // fbterm: its own colour escapes after the attributes (terminfo
            // setaf=\E[1;%p1%d}, setab=\E[2;%p1%d})
            ColorMode::C256 if self.fbterm => {
                let _ = write!(out, "m\x1b[1;{}}}", rgb256(fg));
                if let Some(b) = bg {
                    let _ = write!(out, "\x1b[2;{}}}", rgb256(b));
                }
                return;
            }
            ColorMode::C256 => {
                let _ = write!(out, ";38;5;{}", rgb256(fg));
                if let Some(b) = bg {
                    let _ = write!(out, ";48;5;{}", rgb256(b));
                }
            }
            ColorMode::C16 => {
                let (c, bright) = rgb16(fg);
                let _ = write!(out, ";{}", if bright { 90 + c } else { 30 + c });
                // the Linux console and fbterm brighten via bold, not 90-97
                if bright {
                    out.extend_from_slice(b";1");
                }
                if let Some(b) = bg {
                    let _ = write!(out, ";{}", 40 + rgb16_bg(b));
                }
            }
        }
        out.push(b'm');
    }

    /// Reset attributes to the window background and clear to the end of
    /// the line with it.
    fn clear_eol(&self, out: &mut Vec<u8>) {
        match self.screen_bg {
            None => out.extend_from_slice(b"\x1b[0m\x1b[K"),
            Some(b) => {
                let _ = match self.color {
                    ColorMode::True => write!(out, "\x1b[0;48;2;{};{};{}m\x1b[K", b.0, b.1, b.2),
                    ColorMode::C256 if self.fbterm => write!(out, "\x1b[0m\x1b[2;{}}}\x1b[K", rgb256(b)),
                    ColorMode::C256 => write!(out, "\x1b[0;48;5;{}m\x1b[K", rgb256(b)),
                    ColorMode::C16 => write!(out, "\x1b[0;{}m\x1b[K", 40 + rgb16_bg(b)),
                };
            }
        }
    }

    /// Draw a full frame: text rows, images, the prompt / status line.
    fn render(&mut self, con: &mut Console, res: &mut Resources, req: Option<&InputReq>) {
        // the window background, unless it is the game's default (then the
        // terminal's own background shows)
        self.screen_bg = (con.screen_bg != con.default_bg).then_some(self.recolored(con.screen_bg));
        let (cols, rows, xp, yp) = winsize();
        self.cols = cols;
        self.rows = rows.max(2);
        if !self.fb_cell_fixed && xp > 0 && yp > 0 {
            self.cell = ((xp / cols as u32).max(1), (yp / rows as u32).max(1));
        }
        con.width = cols.saturating_sub(1).max(20);
        self.button_wrap = con.button_wrap;
        let scale = self.cell.1 as f32 / con.line_height.max(1.0);
        let view_h = self.rows - 1;
        self.buttons.clear();
        self.hover_imgs.clear();
        // layout from the bottom up
        let mut all: Vec<DRow> = Vec::new();
        // (plus some rows above the view: images there can reach into it)
        let need = view_h + self.scroll + IMG_REACH;
        let cur_line = if con.cur.is_empty() {
            None
        } else {
            Some(Line {
                segs: con.cur.clone(),
                align: con.cur_align.unwrap_or(con.align),
                gen: con.gen,
                temporary: false,
                cols: self.cols,
                kojo: false,
                src: 0,
                wait: con.waits,
            })
        };
        let mut chunks: Vec<Vec<DRow>> = Vec::new();
        // for each chunk: the key of its line if it is translated (or could
        // be shown so), for clicking between the original and the translation
        let mut chunk_keys: Vec<Option<u64>> = Vec::new();
        let mut count = 0;
        if let Some(l) = &cur_line {
            let r = self.layout_line(l, con.valid_gen, scale);
            count += r.len();
            chunks.push(r);
            chunk_keys.push(None);
        }
        // dialog translated in batches (the lines that may show)
        let blocks = if self.mt.is_some() { self.translate_blocks(con, con.lines.len().saturating_sub(need)) } else { crate::fxhash::HashMap::default() };
        for (i, l) in con.lines.iter().enumerate().rev() {
            if count >= need {
                break;
            }
            let tr = match blocks.get(&i) {
                Some(t) => Some(t.clone()),
                None if self.mt.is_some() => self.translated(l, &con.src_files),
                None => None,
            };
            // a translated line the player clicked shows its original again
            let key = tr.as_ref().map(|_| line_key(l));
            let tr = match key {
                Some(k) if self.show_orig.contains(&k) => None,
                _ => tr,
            };
            let r = self.layout_line(tr.as_ref().unwrap_or(l), con.valid_gen, scale);
            count += r.len();
            chunks.push(r);
            chunk_keys.push(key);
        }
        let mut row_keys: Vec<Option<u64>> = Vec::new();
        for (c, k) in chunks.into_iter().zip(chunk_keys).rev() {
            row_keys.extend(std::iter::repeat(k).take(c.len()));
            all.extend(c);
        }
        // <div> content: painted over the rows it reaches (Emuera draws a
        // div on top of whatever lines are there)
        for idx in 0..all.len() {
            if all[idx].divs.is_empty() {
                continue;
            }
            for d in std::mem::take(&mut all[idx].divs) {
                for (k, r) in d.rows.into_iter().enumerate() {
                    let t = idx as i64 + d.dy as i64 + k as i64;
                    if t < 0 || t as usize >= all.len() {
                        continue;
                    }
                    let t = t as usize;
                    for mut im in r.imgs {
                        im.col += d.col;
                        all[t].imgs.push(im);
                    }
                    paint_cells(&mut all[t], d.col, r.cells);
                }
            }
        }
        // a picture menu (CBG layers) may draw anywhere on the screen:
        // give it rows to draw text into below short content
        if !res.cbg.is_empty() && self.scroll == 0 {
            while all.len() < view_h {
                all.push(DRow::default());
            }
        }
        let max_scroll = all.len().saturating_sub(view_h);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        let end = all.len() - self.scroll.min(all.len());
        let start = end.saturating_sub(view_h);
        let cbg_places = if res.cbg.is_empty() { Vec::new() } else { self.cbg_layers(con, res, &mut all[start..end]) };
        let visible = &all[start..end];
        self.row_lines = (start..end).map(|i| row_keys.get(i).copied().flatten()).collect();

        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(b"\x1b[?25l");
        self.gfx.begin(&mut out);
        // hover button id (from the previous frame's map; ids are stable)
        let hover = self.mouse.and_then(|(x, y)| self.button_at(x, y));
        self.hover = hover;
        self.btn_map = vec![Vec::new(); self.rows];
        self.frame = vec![Vec::new(); self.rows];
        let mut placements = cbg_places;
        // the console mouse (gpm or the raw device) has no pointer of its
        // own on screen: draw one
        // (blinking: off half the time)
        let pointer = if self.own_pointer() && self.ptr_on { self.mouse } else { None };
        let mut pointer_drawn = false;
        // each row's bytes, for damage tracking (see `skip_unchanged_rows`)
        let mut row_bytes: Vec<(usize, usize)> = Vec::with_capacity(view_h);
        let rows_from = out.len();
        for (sr, row) in visible.iter().enumerate() {
            let row_start = out.len();
            let _ = write!(out, "\x1b[{};1H", sr + 1);
            let mut col = 0usize;
            let mut last_style: Option<(Style, Option<Rgb>)> = None;
            let mut btn_start: Option<(usize, usize)> = None;
            for cell in &row.cells {
                // clip rows printed when the screen was wider
                if col + cell.w as usize > cols {
                    break;
                }
                if let Some((s, b)) = btn_start {
                    if cell.button != Some(b) {
                        self.btn_map[sr].push((s, col, b));
                        btn_start = None;
                    }
                }
                if let Some(b) = cell.button {
                    if btn_start.is_none() {
                        btn_start = Some((col, b));
                    }
                }
                let ov = match (cell.button, hover) {
                    (Some(b), Some(h)) if b == h && self.is_button(b) => Some(con.focus_fg),
                    _ => None,
                };
                let key = (cell.style.clone(), ov);
                if last_style.as_ref() != Some(&key) {
                    self.sgr(&mut out, &cell.style, ov);
                    last_style = Some(key);
                }
                self.frame[sr].push((col, cell.clone(), ov));
                let mut tmp = [0u8; 4];
                let at_pointer = pointer.is_some_and(|(px, py)| py == sr && px >= col && px < col + cell.w.max(1) as usize);
                if at_pointer {
                    if cell.ch.is_whitespace() {
                        let blank = Style { fg: self.pointer_fg(cell.style.bg), ..cell.style.clone() };
                        self.sgr(&mut out, &blank, None);
                        last_style = None;
                    }
                    out.extend_from_slice(b"\x1b[7m");
                    pointer_drawn = true;
                }
                if self.ambi == Ambi::Osc66 && cell.w == 2 && UnicodeWidthChar::width(cell.ch) == Some(1) {
                    // a one-column glyph drawn across two cells (see char_cells)
                    let _ = write!(out, "\x1b]66;w=2;{}\x1b\\", cell.ch);
                } else {
                    out.extend_from_slice(cell.ch.encode_utf8(&mut tmp).as_bytes());
                }
                if at_pointer {
                    out.extend_from_slice(b"\x1b[27m");
                }
                col += cell.w as usize;
            }
            if let Some((s, b)) = btn_start {
                self.btn_map[sr].push((s, col, b));
            }
            self.clear_eol(&mut out);
            row_bytes.push((row_start, out.len()));
        }
        // images: placed by pixel, so a picture can start rows above or
        // below its own line (negative `ypos` stacks portrait layers, each
        // printed on the next line, back onto the first one) and is cropped
        // to the view
        let (ch, view_px) = (self.cell.1.max(1) as i32, (view_h as u32 * self.cell.1.max(1)) as i32);
        let mut pending: Vec<(String, u32, u32, i32, i32, u32, u32)> = Vec::new();
        for idx in start.saturating_sub(IMG_REACH)..all.len().min(end + IMG_REACH) {
            let sr = idx as i32 - start as i32;
            for im in all[idx].imgs.iter().filter(|im| im.col < cols) {
                let pw = (im.w * scale).round().max(1.0) as u32;
                let ph = (im.h * scale).round().max(1.0) as u32;
                let top = sr * ch + (im.ypos * scale).round() as i32;
                if top + ph as i32 <= 0 || top >= view_px {
                    continue;
                }
                let crop_top = (-top).max(0) as u32;
                let top = top.max(0);
                let crop_h = (ph - crop_top).min((view_px - top) as u32);
                if crop_h == 0 {
                    continue;
                }
                // a picture in a button: all of it is the button (hover,
                // tooltip, click), as in Emuera, not just its text row
                if let Some(b) = im.button {
                    let cw = self.cell.0.max(1) as usize;
                    let (c0, c1) = (im.col, (im.col + (pw as usize).div_ceil(cw)).min(cols));
                    let (r0, r1) = (top as usize / ch as usize, (top as usize + crop_h as usize).div_ceil(ch as usize).min(view_h));
                    for r in r0..r1 {
                        if let Some(m) = self.btn_map.get_mut(r) {
                            m.push((c0, c1, b));
                        }
                    }
                }
                // images off (or hidden under the overlay): nothing is decoded
                // or scaled, so no picture is read until a backend is chosen
                if self.no_images || self.gfx.name() == "none" {
                    continue;
                }
                // srcb: the hover picture while the pointer is on its button
                let name = match &im.hover {
                    Some(h) if im.button.is_some() && im.button == self.hover => h,
                    _ => &im.name,
                };
                pending.push((res.frame_name(name), pw, ph, im.col as i32, top, crop_top, crop_h));
            }
        }
        // pictures not scaled yet are scaled on all cores at once (a status
        // screen shows several large portrait layers)
        res.prescale(pending.iter().map(|p| (p.0.as_str(), p.1, p.2)).collect());
        // an animated sprite on screen: the input wait redraws it
        self.animating = res.take_anim_used();
        for (name, pw, ph, col, top, crop_top, crop_h) in pending {
            if let Some(img) = res.scaled(&name, pw, ph) {
                placements.push(Placement {
                    key: format!("{}@{}x{}#{}", name, pw, ph, res.version(&name)),
                    img,
                    col,
                    row: top / ch,
                    y_off: top % ch,
                    crop_top,
                    crop_h,
                    below: false,
                });
            }
        }
        for sr in visible.len()..view_h {
            let row_start = out.len();
            let _ = write!(out, "\x1b[{};1H", sr + 1);
            self.clear_eol(&mut out);
            row_bytes.push((row_start, out.len()));
        }
        let rows_to = out.len();
        if let (Some((px, py)), false) = (pointer, pointer_drawn) {
            // pointer over blank space
            if py < view_h {
                let _ = write!(out, "\x1b[{};{}H", py + 1, px + 1);
                let blank = Style { fg: self.pointer_fg(None), bg: None, bold: false, italic: false, underline: false, strike: false };
                self.sgr(&mut out, &blank, None);
                out.extend_from_slice(b"\x1b[7m \x1b[0m");
            }
        }
        // status / input line
        let _ = write!(out, "\x1b[{};1H", self.rows);
        self.clear_eol(&mut out);
        let prompt = match req {
            Some(r) if r.kind == InputKind::Wait => "\x1b[2m▼ (Enter)\x1b[0m".to_string(),
            Some(_) if self.buf.is_empty() && self.notice.is_some() => format!("\x1b[1m>\x1b[0m \x1b[2m({})\x1b[0m", self.notice.as_deref().unwrap_or("")),
            Some(_) => format!("\x1b[1m>\x1b[0m {}", self.buf),
            None => String::new(),
        };
        out.extend_from_slice(prompt.as_bytes());
        if req.is_some() {
            self.busy_since = None;
        }
        self.bar = None;
        if self.scroll == 0 && cols > 40 && req.is_some() && !self.no_images {
            let used = 2 + self.buf.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>() + 20;
            self.bar = Some((used, con.running.clone(), 0));
            self.draw_bar(&mut out, hover);
        }
        if self.scroll > 0 {
            let tag = format!(" [scroll -{}] ", self.scroll);
            let _ = write!(out, "\x1b[{};{}H\x1b[7m{}\x1b[0m", self.rows, cols.saturating_sub(tag.len()), tag);
        }
        // overlapping layers (portraits) go to the terminal as one picture
        let mut placements = self.merger.merge(placements, self.cell, self.gfx.name() == "kitty-tmux");
        // buttons with tooltips need a full redraw when hovered, and the
        // hovered one shows its tooltip by the pointer
        for (i, v) in self.buttons.iter().enumerate() {
            if con.tooltip_for(v).is_some() {
                self.hover_imgs.insert(i);
            }
        }
        // the tooltip shows once the pointer has rested on its button for
        // TOOLTIP_SETDELAY, until TOOLTIP_SETDURATION has gone by
        let has_tip = |b: usize| self.buttons.get(b).and_then(|v| con.tooltip_for(v)).is_some();
        match (hover.filter(|&b| has_tip(b)), self.tip_since) {
            (Some(b), Some((t, _))) if t == b => {}
            (Some(b), _) => self.tip_since = Some((b, Instant::now())),
            (None, _) => self.tip_since = None,
        }
        let tip = match (hover, self.mouse) {
            (Some(b), Some((mx, my))) if self.tip_due(con).0 => self.buttons.get(b).and_then(|v| con.tooltip_for(v)).map(|t| self.tooltip_box(t, mx, my)),
            _ => None,
        };
        self.tip_drawn = tip.is_some();
        // pictures sit above text: the tooltip's place is cut out of them
        if let Some(t) = &tip {
            let cells = (t.left as i64 - 1, t.top as i64 - 1, (t.left + t.w) as i64 - 1, (t.top + t.lines.len()) as i64 - 1);
            placements = placements.into_iter().map(|p| self.merger.cut(p, cells, self.cell)).collect();
        }
        if self.force_frame {
            self.gfx.reset(&mut out);
        }
        self.gfx.draw(&mut out, &placements, self.cell);
        let mut tip_rows = 0..0;
        if let Some(t) = &tip {
            tip_rows = self.draw_tooltip(&mut out, t, con.tip_colors);
        }
        self.drawn_pointer = pointer.filter(|p| p.1 < view_h);
        if self.mouse_log.is_some() {
            let (m, d, own) = (self.mouse, self.drawn_pointer, self.own_pointer());
            self.mlog(&format!("frame: mouse={:?} pointer drawn at {:?} (ours={} on={})", m, d, own, self.ptr_on));
        }
        self.frame_cursor = None;
        // cursor at end of input
        if req.map(|r| r.kind != InputKind::Wait).unwrap_or(false) {
            let w: usize = self.buf.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum();
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", self.rows, 3 + w);
            self.frame_cursor = Some((self.rows, 3 + w));
        }
        // a frame exactly like the last one (a script redrawing a picture
        // menu every few milliseconds) isn't sent again
        let fp = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            out.hash(&mut h);
            for p in &placements {
                p.key.hash(&mut h);
            }
            // (a pointer drawn as an arrow isn't in the text)
            self.drawn_pointer.hash(&mut h);
            h.finish()
        };
        // (unless a box drawn over it has closed: what it covered must
        // come back)
        if fp == self.last_frame && !self.force_frame && !self.box_closed.get() {
            self.frame_imgs = placements;
            con.dirty = false;
            return;
        }
        let out = self.skip_unchanged_rows(out, (rows_from, rows_to), &row_bytes, &placements, cols, tip_rows);
        if std::env::var_os("EMUERA_DEBUG_FRAMES").is_some() {
            eprintln!("frame {:x} len {} keys {:?}", fp, out.len(), placements.iter().map(|p| p.key.clone()).collect::<Vec<_>>());
        }
        self.last_frame = fp;
        self.force_frame = false;
        write_all(&out);
        crate::sys::drain_stdout();
        self.gfx.after_flush(&placements, self.cell);
        self.pointer_over_images(&placements);
        self.frame_imgs = placements;
        con.dirty = false;
    }

    /// The client background layers (CBG*): as images where the terminal
    /// shows them, else the text drawn into them (GDRAWTEXT) painted into
    /// the screen `rows`, so a menu drawn as a picture still works.
    /// Returns their image placements, back to front.
    fn cbg_layers(&mut self, con: &Console, res: &mut Resources, rows: &mut [DRow]) -> Vec<Placement> {
        use crate::resources::CbgSrc;
        let cw = (con.font_size / 2.0).max(1.0);
        let lh = con.line_height.max(1.0);
        let client_h = self.rows as f32 * lh;
        // the button under the mouse (button map colour)
        let hovered = self.mouse.and_then(|(c, r)| {
            let x = (c as f32 * cw + cw / 2.0) as i64;
            let y = (r as f32 * lh + lh / 2.0 - client_h) as i64;
            res.cbg_button_at(x, y)
        });
        let images = self.gfx.name() != "none" && !self.no_images;
        let (sx, sy) = (self.cell.0 as f32 / cw, self.cell.1 as f32 / lh);
        let screen_px = (self.rows as u32 * self.cell.1.max(1)) as i32;
        let mut out = Vec::new();
        for l in res.cbg.clone() {
            let src = match &l.button {
                Some((v, hover)) if hovered == Some(*v) => match hover {
                    Some(h) => h.clone(),
                    None => continue,
                },
                _ => l.src.clone(),
            };
            if let CbgSrc::Sprite(n) = &src {
                if n.is_empty() {
                    continue;
                }
            }
            if !images {
                // (only the size is needed: no pixel fingerprint)
                let CbgSrc::G(gid) = src else { continue };
                let Some(g) = res.graphics.get(&gid) else { continue };
                let top_e = client_h + l.y as f32 - g.height() as f32;
                // text fallback: what GDRAWTEXT wrote into the buffer
                for t in &g.texts {
                    // text drawn larger than the console font takes more
                    // pixels per character than a cell: keep its middle
                    // where it was (menus are centred), not its left edge
                    let text = t.text.trim();
                    let lead = t.text.len() - t.text.trim_start().len();
                    let half = t.size as f32 / 2.0; // px per half-width char
                    let w_px = crate::loader::sjis_len(text) as f32 * half;
                    let mid = l.x as f32 + t.x as f32 + lead as f32 * half + w_px / 2.0;
                    let st = Style {
                        fg: Rgb(t.color[0], t.color[1], t.color[2]),
                        bg: None,
                        bold: t.size as f32 > con.font_size,
                        italic: false,
                        underline: false,
                        strike: false,
                    };
                    let cells = self.text_cells(text, &st);
                    let width: usize = cells.iter().map(|c| c.w as usize).sum();
                    let col = (mid / cw - width as f32 / 2.0).round().max(0.0);
                    let row = ((top_e + t.y as f32 + t.size as f32 / 2.0) / lh).floor();
                    if row < 0.0 || row as usize >= rows.len() || text.is_empty() {
                        continue;
                    }
                    paint_cells(&mut rows[row as usize], col as usize, cells);
                }
                continue;
            }
            let Some((img, fp)) = res.cbg_pixels(&src) else { continue };
            let (w, h) = (img.width() as f32, img.height() as f32);
            // a sprite is offset by its base position (CSV columns 7-8)
            let (bx, by) = match &src {
                CbgSrc::Sprite(n) => res.base_pos(n),
                CbgSrc::G(_) => (0, 0),
            };
            let l = crate::resources::CbgLayer { x: l.x + bx as i32, y: l.y + by as i32, ..l.clone() };
            let top_e = client_h + l.y as f32 - h;
            let (pw, ph) = ((w * sx).round().max(1.0) as u32, (h * sy).round().max(1.0) as u32);
            let (left, top) = ((l.x as f32 * sx).round() as i32, (top_e * sy).round() as i32);
            if left < 0 || top + ph as i32 <= 0 || top >= screen_px {
                continue;
            }
            let crop_top = (-top).max(0) as u32;
            let top_c = top.max(0);
            let crop_h = (ph - crop_top.min(ph)).min((screen_px - top_c) as u32);
            if crop_h == 0 {
                continue;
            }
            let key = (fp, pw, ph);
            let scaled = match self.cbg_scaled.get(&key) {
                Some(i) => i.clone(),
                None => {
                    if self.cbg_scaled.len() > 8 {
                        self.cbg_scaled.clear();
                    }
                    let i = if (pw, ph) == img.dimensions() {
                        img.clone()
                    } else {
                        std::rc::Rc::new(image::imageops::resize(&*img, pw, ph, image::imageops::FilterType::Triangle))
                    };
                    self.cbg_scaled.insert(key, i.clone());
                    i
                }
            };
            let ch = self.cell.1.max(1) as i32;
            out.push(Placement {
                key: format!("cbg{:x}@{}x{}", fp, pw, ph),
                img: scaled,
                col: left / self.cell.0.max(1) as i32,
                row: top_c / ch,
                y_off: top_c % ch,
                crop_top,
                crop_h,
                below: l.z > 0 && self.screen_bg.is_none(),
            });
        }
        out
    }

    /// A character as the screen can draw it (`char_cells`' cells): on the
    /// Linux console one its font lacks becomes a lookalike, in the same
    /// cells (the lookalike in the first, filler after).
    fn drawable(&self, ch: char, tw: usize, extra: usize, fill: char) -> (char, usize, usize, char) {
        match &self.font_chars {
            Some(has) if tw > 0 && !ch.is_ascii() && !has.contains(&ch) => {
                let c = console_lookalike(ch, has);
                if c == ch {
                    return (ch, tw, extra, fill);
                }
                (c, 1, tw + extra - 1, if fill == ch { c } else { ' ' })
            }
            _ => (ch, tw, extra, fill),
        }
    }

    /// Cells for a run of text in one style (for text painted over rows).
    fn text_cells(&self, text: &str, st: &Style) -> Vec<Cell> {
        let mut v = Vec::new();
        for ch in text.chars() {
            let (tw, extra, fill) = char_cells(ch, self.ambi, self.widths.get(&ch).copied());
            let (ch, tw, extra, fill) = self.drawable(ch, tw, extra, fill);
            if tw == 0 {
                continue;
            }
            v.push(Cell { ch, w: tw as u8, style: st.clone(), button: None });
            for _ in 0..extra {
                v.push(Cell { ch: fill, w: 1, style: st.clone(), button: None });
            }
        }
        v
    }

    /// A tooltip box next to the pointer at 0-based cell (`mx`, `my`):
    /// below it, or above when there is no room.
    /// Where a tooltip goes and its lines (see [`Tty::draw_tooltip`]).
    fn tooltip_box(&self, tip: &str, mx: usize, my: usize) -> TipBox {
        let maxw = self.cols.saturating_sub(4).clamp(10, 80);
        let mut lines: Vec<(String, usize)> = Vec::new();
        // games break tooltip lines with CR (`UNICODE(0x000D)`, AnonTW's
        // map) as well as LF; written raw, a CR sends the cursor to the
        // left edge and the rest lands over the screen
        let mut tip = tip.replace("\r\n", "\n");
        // Emuera turns `<br>` into a line break too
        for br in ["<br>", "<BR>", "<br/>", "<br />"] {
            tip = tip.replace(br, "\n");
        }
        let tip = printable(&tip);
        for para in tip.split(['\r', '\n']) {
            let (mut cur, mut w) = (String::new(), 0usize);
            for word in para.split(' ') {
                let ww: usize = word.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum();
                if w > 0 && w + 1 + ww > maxw {
                    lines.push((std::mem::take(&mut cur), w));
                    w = 0;
                }
                if w > 0 {
                    cur.push(' ');
                    w += 1;
                }
                for c in word.chars() {
                    let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                    if w + cw > maxw {
                        lines.push((std::mem::take(&mut cur), w));
                        w = 0;
                    }
                    cur.push(c);
                    w += cw;
                }
            }
            lines.push((cur, w));
        }
        // as tall as the screen allows (Emuera shows all of it: AnonTW's
        // ability descriptions run to a dozen lines); a cut one ends in …
        let most = self.rows.saturating_sub(2).max(1);
        if lines.len() > most {
            lines.truncate(most);
            if let Some((l, w)) = lines.last_mut() {
                if *w + 1 >= maxw {
                    l.pop();
                } else {
                    *w += 1;
                }
                l.push('…');
            }
        }
        let w = lines.iter().map(|l| l.1).max().unwrap_or(0) + 2;
        let h = lines.len();
        let view = self.rows.saturating_sub(1);
        let top = if my + 2 + h <= view { my + 2 } else { (my + 1).saturating_sub(h).max(1) };
        let left = (mx + 2).min(self.cols.saturating_sub(w)).max(1);
        TipBox { lines, top, left, w }
    }

    /// Should the hovered button's tooltip show now, and when that next
    /// changes (its delay or duration running out).
    fn tip_due(&self, con: &Console) -> (bool, Option<Instant>) {
        let Some((_, t0)) = self.tip_since else { return (false, None) };
        let (delay, dur) = (Duration::from_millis(con.tip_delay), Duration::from_millis(con.tip_duration));
        let e = t0.elapsed();
        if e < delay {
            (false, Some(t0 + delay))
        } else if con.tip_duration == 0 {
            (true, None)
        } else if e < delay + dur {
            (true, Some(t0 + delay + dur))
        } else {
            (false, None)
        }
    }

    /// Write a tooltip box; returns the screen rows (0-based) it covers.
    fn draw_tooltip(&self, out: &mut Vec<u8>, t: &TipBox, colors: Option<(Rgb, Rgb)>) -> std::ops::Range<usize> {
        let (fg, bg) = colors.map_or(((20, 20, 20), (255, 245, 200)), |(f, b)| ((f.0, f.1, f.2), (b.0, b.1, b.2)));
        let st = self.ov_style(fg, bg, false);
        for (i, (s, sw)) in t.lines.iter().enumerate() {
            let _ = write!(out, "\x1b[{};{}H{} {}{} \x1b[0m", t.top + i, t.left, st, s, " ".repeat(t.w - 2 - sw));
        }
        t.top - 1..t.top - 1 + t.lines.len()
    }

    /// Damage tracking: drop from `out` the rows (byte ranges `rows` within
    /// `out`'s `span`) that are the same as on the terminal already. A row
    /// is written anyway after anything else drew over it (an overlay, a
    /// tooltip, a hover repaint), on a forced or resized frame, and where
    /// the pictures changed for backends that draw them into the cells
    /// (framebuffer, sixel, iTerm2), so old pixels go.
    #[allow(clippy::too_many_arguments)]
    fn skip_unchanged_rows(
        &mut self,
        out: Vec<u8>,
        span: (usize, usize),
        rows: &[(usize, usize)],
        placements: &[Placement],
        cols: usize,
        tip_rows: std::ops::Range<usize>,
    ) -> Vec<u8> {
        use std::hash::{Hash, Hasher};
        let full =
            self.force_frame || self.redraw_all.replace(false) | self.box_closed.replace(false) || self.row_hash.len() != rows.len() || self.hash_cols != cols;
        let ch = self.cell.1.max(1) as i32;
        // rows under pictures, before and now, when the pictures changed
        let mut img_rows = vec![false; rows.len()];
        let cell_images = !matches!(self.gfx.name(), "kitty" | "none");
        let changed = placements.len() != self.frame_imgs.len()
            || placements
                .iter()
                .zip(&self.frame_imgs)
                .any(|(a, b)| a.key != b.key || (a.col, a.row, a.y_off, a.crop_top, a.crop_h) != (b.col, b.row, b.y_off, b.crop_top, b.crop_h));
        if cell_images && changed {
            for p in placements.iter().chain(&self.frame_imgs) {
                let last = p.row + (p.y_off + p.crop_h as i32 + ch - 1) / ch;
                for r in p.row.max(0)..last.max(0) {
                    if let Some(f) = img_rows.get_mut(r as usize) {
                        *f = true;
                    }
                }
            }
        }
        let mut hashes = Vec::with_capacity(rows.len());
        let mut kept = Vec::with_capacity(out.len());
        kept.extend_from_slice(&out[..span.0]);
        for (r, &(a, b)) in rows.iter().enumerate() {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            out[a..b].hash(&mut h);
            let h = h.finish() | 1;
            if full || img_rows[r] || self.row_hash.get(r) != Some(&h) {
                kept.extend_from_slice(&out[a..b]);
            }
            hashes.push(h);
        }
        kept.extend_from_slice(&out[span.1..]);
        // the tooltip covers these rows now: write them again next time
        for r in tip_rows {
            if let Some(h) = hashes.get_mut(r) {
                *h = 0;
            }
        }
        self.row_hash = hashes;
        self.hash_cols = cols;
        kept
    }

    /// The clipboard options in force: emuera.config's, unless the
    /// settings force the feature on or off.
    fn clip_config(&self, con: &Console) -> crate::clipboard::ClipConfig {
        let mut c = con.clip.clone();
        match self.clip_mode.as_str() {
            "on" => c.enabled = true,
            "off" => c.enabled = false,
            _ => {}
        }
        c
    }

    /// Copy the printed text to the clipboard if `t` is a trigger for it.
    fn copy_text(&mut self, con: &Console, t: crate::clipboard::Trigger) {
        let cfg = self.clip_config(con);
        if let Some(seq) = self.clip.check(con, &cfg, t) {
            write_all(&seq);
        }
    }

    /// A mouse press may trigger a copy (left, middle or double click).
    /// Click on screen row `y` (0-based): if it belongs to a translated
    /// line, show that line's original text instead (or the translation
    /// again). Whether anything changed.
    fn toggle_translation(&mut self, y: usize) -> bool {
        let Some(Some(k)) = self.row_lines.get(y).copied() else { return false };
        if !self.show_orig.remove(&k) {
            self.show_orig.insert(k);
        }
        self.force_frame = true;
        true
    }

    fn click_copies(&mut self, con: &Console, btn: u16, x: u16, y: u16) {
        use crate::clipboard::Trigger;
        match btn & !(4 | 8 | 16) {
            0 => {
                let double = self.last_click.is_some_and(|(t, p)| t.elapsed() < Duration::from_millis(400) && p == (x, y));
                self.last_click = Some((Instant::now(), (x, y)));
                self.copy_text(con, if double { Trigger::DoubleClick } else { Trigger::LeftClick });
            }
            1 => self.copy_text(con, Trigger::MiddleClick),
            _ => {}
        }
    }

    /// Emuera-Anchor's hotkeys (asking first, as Emuera does, for the two
    /// that lose the game's progress). None: cancelled.
    fn anchor_hotkey(&mut self, con: &mut Console, res: &mut Resources, req: &InputReq, k: &Key) -> Option<InputResult> {
        match k {
            Key::Title => {
                if !self.confirm(con, res, req, "Back to the title screen? Unsaved progress is lost.") {
                    return None;
                }
                let cfg = self.clip_config(con);
                self.clip.reset(con, &cfg);
                Some(InputResult::Hotkey(crate::frontend::Hotkey::Title))
            }
            Key::Restart => {
                if !self.confirm(con, res, req, "Restart the game? Unsaved progress is lost.") {
                    return None;
                }
                self.shutdown();
                let e = crate::sys::exec_self(&std::env::args_os().skip(1).collect::<Vec<_>>());
                eprintln!("emuera-tty: can't restart: {}", e);
                Some(InputResult::Quit)
            }
            _ => Some(InputResult::Hotkey(crate::frontend::Hotkey::Reload)),
        }
    }

    /// Does the pointer's move change which hover picture (`srcb`) shows?
    fn hover_swaps_image(&self) -> bool {
        let new = self.mouse.and_then(|(x, y)| self.button_at(x, y));
        new != self.hover && [new, self.hover].iter().flatten().any(|b| self.hover_imgs.contains(b))
    }

    /// After a mouse move, repaint only what changed from the last frame:
    /// the old and new console pointer cells and the cells of the button
    /// that lost or gained the hover highlight (re-blitting framebuffer
    /// images under them). A full redraw makes the images flicker.
    fn repaint_hover(&mut self, focus: Rgb) {
        let new_ptr = if self.own_pointer() && self.ptr_on { self.mouse.filter(|p| p.1 < self.rows.saturating_sub(1)) } else { None };
        let new_hover = self.mouse.and_then(|(x, y)| self.button_at(x, y));
        let old_hover = self.hover;
        if new_ptr == self.drawn_pointer && new_hover == old_hover {
            return;
        }
        let hits = |pos: Option<(usize, usize)>, r: usize, c: usize, w: usize| pos.is_some_and(|(px, py)| py == r && px >= c && px < c + w);
        let mut out: Vec<u8> = Vec::with_capacity(1024);
        out.extend_from_slice(b"\x1b[?25l");
        let mut touched = Vec::new();
        let mut covered = [false, false];
        for r in 0..self.frame.len() {
            for i in 0..self.frame[r].len() {
                let (c, ref cell, _) = self.frame[r][i];
                let w = cell.w.max(1) as usize;
                let on_old = hits(self.drawn_pointer, r, c, w);
                let on_new = hits(new_ptr, r, c, w);
                covered[0] |= on_old;
                covered[1] |= on_new;
                let btn_changed = new_hover != old_hover && cell.button.is_some() && (cell.button == old_hover || cell.button == new_hover);
                if !(on_old || on_new || btn_changed) {
                    continue;
                }
                let ov = match (cell.button, new_hover) {
                    (Some(b), Some(h)) if b == h && self.is_button(b) => Some(focus),
                    _ => None,
                };
                let _ = write!(out, "\x1b[{};{}H\x1b[0m", r + 1, c + 1);
                if on_new && cell.ch.is_whitespace() {
                    let blank = Style { fg: self.pointer_fg(cell.style.bg), ..cell.style.clone() };
                    self.sgr(&mut out, &blank, None);
                } else {
                    self.sgr(&mut out, &cell.style, ov);
                }
                if on_new {
                    out.extend_from_slice(b"\x1b[7m");
                }
                let mut tmp = [0u8; 4];
                if self.ambi == Ambi::Osc66 && cell.w == 2 && UnicodeWidthChar::width(cell.ch) == Some(1) {
                    let _ = write!(out, "\x1b]66;w=2;{}\x1b\\", cell.ch);
                } else {
                    out.extend_from_slice(cell.ch.encode_utf8(&mut tmp).as_bytes());
                }
                out.extend_from_slice(b"\x1b[0m");
                touched.push((c, r, w));
                self.frame[r][i].2 = ov;
            }
        }
        // pointer over blank space past the end of a row
        for (k, (pos, inverse)) in [(self.drawn_pointer, false), (new_ptr, true)].into_iter().enumerate() {
            if let (Some((px, py)), false) = (pos, covered[k]) {
                let _ = write!(out, "\x1b[{};{}H", py + 1, px + 1);
                // a blank in the window background, inverted for the pointer
                let blank = Style { fg: self.pointer_fg(None), bg: None, bold: false, italic: false, underline: false, strike: false };
                self.sgr(&mut out, &blank, None);
                out.extend_from_slice(if inverse { b"\x1b[7m \x1b[0m" } else { b" \x1b[0m" });
                touched.push((px, py, 1));
            }
        }
        // (on the Linux console every move, for the mouse counter there)
        let console = self.term == "linux" || self.term.starts_with("fbterm");
        if self.status_info && (new_hover != old_hover || console) {
            self.draw_bar(&mut out, new_hover);
        }
        if let Some((r, c)) = self.frame_cursor {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", r, c);
        }
        write_all(&out);
        crate::sys::drain_stdout();
        for &(_, r, _) in &touched {
            if let Some(h) = self.row_hash.get_mut(r) {
                *h = 0;
            }
        }
        if self.mouse_log.is_some() {
            let d = self.drawn_pointer;
            self.mlog(&format!("repaint: pointer {:?} -> {:?}, hover {:?} -> {:?}", d, new_ptr, old_hover, new_hover));
        }
        self.drawn_pointer = new_ptr;
        self.hover = new_hover;
        // images the repainted cells cut into (only the framebuffer draws
        // over text: terminal images sit above it)
        let (cw, ch) = (self.cell.0.max(1) as i32, self.cell.1.max(1) as i32);
        let hit: Vec<Placement> = self
            .frame_imgs
            .iter()
            .filter(|p| {
                let (x0, y0) = (p.col * cw, p.row * ch + p.y_off);
                let (x1, y1) = (x0 + p.img.width() as i32, y0 + p.crop_h as i32);
                touched.iter().any(|&(c, r, w)| {
                    let (cx0, cy0) = (c as i32 * cw, r as i32 * ch);
                    cx0 < x1 && cx0 + w as i32 * cw > x0 && cy0 < y1 && cy0 + ch > y0
                })
            })
            .cloned()
            .collect();
        if !hit.is_empty() {
            self.gfx.after_flush(&hit, self.cell);
            self.pointer_over_images(&hit);
        }
    }

    /// The colour of the pointer over blank space (it shows inverted, as a
    /// block): light on a dark background `bg` (else the window's), dark on
    /// a light one. Not the blank's own text colour: that may be the
    /// background's (black on the console's black, so it vanished).
    fn pointer_fg(&self, bg: Option<Rgb>) -> Rgb {
        match bg.or(self.screen_bg) {
            Some(Rgb(r, g, b)) if r as u32 * 299 + g as u32 * 587 + b as u32 * 114 > 128_000 => Rgb(0, 0, 0),
            _ => Rgb(204, 204, 204),
        }
    }

    /// The framebuffer paints pictures over the text, the pointer's
    /// inverted cell included: invert it again in pixels when it is under
    /// one of the pictures `ps` just painted.
    fn pointer_over_images(&mut self, ps: &[Placement]) {
        let Some((px, py)) = self.drawn_pointer else { return };
        let (cw, ch) = (self.cell.0.max(1) as i32, self.cell.1.max(1) as i32);
        let (cx, cy) = (px as i32 * cw, py as i32 * ch);
        // (where they are see-through the text's inverted cell shows)
        let mut opaque = 0;
        for y in cy..cy + ch {
            for x in cx..cx + cw {
                opaque += ps.iter().any(|p| {
                    let (ix, iy) = (x - p.col * cw, y - (p.row.max(0) * ch + p.y_off) + p.crop_top as i32);
                    ix >= 0
                        && iy >= p.crop_top as i32
                        && iy < (p.crop_top + p.crop_h) as i32
                        && (ix as u32) < p.img.width()
                        && (iy as u32) < p.img.height()
                        && p.img.get_pixel(ix as u32, iy as u32).0[3] >= 128
                }) as i32;
            }
        }
        if opaque * 2 > cw * ch {
            self.gfx.invert_cell(px, py, self.cell);
        }
    }

    /// The settings overlay (F1 / Ctrl-O): a box over the game listing
    /// `settings::ITEMS`, changed with ← / → / Enter and saved at once;
    /// most apply to the running game. Images are hidden while it is up
    /// (kitty would draw them over it).
    fn settings_overlay(&mut self, con: &mut Console, res: &mut Resources, req: &InputReq) -> OverlayExit {
        use crate::settings::{Settings, ENV_VARS, ITEMS};
        let mut st = Settings::load();
        let seed_row = format!("{:<18} {}  (read-only)", "Random seed", crate::vm::CURRENT_SEED.load(std::sync::atomic::Ordering::Relaxed));
        // the variables page is only offered when one of its variables is set
        let env_any = ENV_VARS.iter().any(|(name, _)| std::env::var_os(name).is_some());
        let mut sel = 0usize;
        let mut note = String::new();
        // a translator test running: (its answer, when it began, what is tested)
        let mut test: Option<(std::sync::mpsc::Receiver<Result<String, String>>, Instant, String)> = None;
        // an action row's id in the `match` below (the seed row has none)
        const SEED_ROW: usize = usize::MAX;
        let exit = loop {
            // the settings shown (the translation ones hide while the
            // Translator is off) and the rows after them: (text, action id)
            let vis: Vec<usize> = ITEMS.iter().enumerate().filter(|(_, it)| st.shown(it)).map(|(i, _)| i).collect();
            let mut rows: Vec<(&str, usize)> = vec![(seed_row.as_str(), SEED_ROW)];
            if env_any {
                rows.push(("Environment variables…  (the EMUERA_* ones emuera-tty reads)", 0));
            }
            rows.push(("Game settings…  (Emuera's settings window: emuera.config)", 1));
            rows.push(("Manage saves…  (copy, export, back up, restore)", 2));
            if st.get("translator") != "off" {
                rows.push(("Test the translator…  (translates a sample line)", 3));
            }
            rows.extend([
                ("Clear the translation cache  (translate the screen again)", 4),
                ("Clear the picture cache  (frees decoded sprites; they load again when shown)", 5),
                ("Recolor…  (draw a colour of the game's as another)", 12),
                ("Back to the title  (Ctrl-T)", 6),
                ("Restart the game  (Ctrl-R)", 7),
                ("Reload ERB files  (F5 / Ctrl-E)", 8),
                ("Switch game…  (unsaved progress is lost)", 9),
                ("Quit emuera-tty", 10),
                ("Close  (Esc)", 11),
            ]);
            let extra: Vec<&str> = rows.iter().map(|r| r.0).collect();
            let n = vis.len() + extra.len();
            sel = sel.min(n - 1);
            if let Some((rx, began, what)) = &test {
                let secs = began.elapsed().as_secs_f32();
                note = match rx.try_recv() {
                    Ok(Ok(t)) => format!("works ({:.1} s, {}): {}", secs, what, t),
                    Ok(Err(e)) => format!("failed ({:.1} s, {}): {}", secs, what, e),
                    Err(std::sync::mpsc::TryRecvError::Empty) => format!("testing {}… {:.0} s", what, secs),
                    Err(_) => format!("{} stopped", what),
                };
                if !note.starts_with("testing") {
                    test = None;
                }
            }
            self.no_images = true;
            self.render(con, res, Some(req));
            let geo = self.draw_overlay(&st, &vis, &extra, sel, &note);
            let Some(k) = self.next_key(500) else {
                if self.hangup {
                    break OverlayExit::Quit;
                }
                continue;
            };
            note.clear();
            // a click picks a row (and acts on it)
            let mut act = 0i32; // 1 next / Enter, -1 previous
            match k {
                Key::Up | Key::Char('k') => sel = (sel + n - 1) % n,
                Key::Down | Key::Char('j') => sel = (sel + 1) % n,
                Key::Esc | Key::Settings | Key::Char('q') => break OverlayExit::Close,
                Key::CtrlC => break OverlayExit::Quit,
                Key::Enter | Key::Right | Key::Char(' ') | Key::Char('l') => act = 1,
                Key::Left | Key::Char('h') | Key::Backspace => act = -1,
                Key::Mouse { btn, y, press: true, .. } => match btn & !(4 | 8 | 16) {
                    64 => sel = (sel + n - 1) % n,
                    65 => sel = (sel + 1) % n,
                    0 => {
                        let row = y as usize;
                        if row > geo.0 && row <= geo.0 + n {
                            sel = row - geo.0 - 1;
                            act = 1;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            if act == 0 {
                continue;
            }
            if sel < vis.len() {
                let it = &ITEMS[vis[sel]];
                let v = st.cycle(it.key, act).to_string();
                st.save();
                if it.live {
                    self.apply_setting(it.key, &v, &st);
                    if it.key == "images" {
                        // off: let go of the pictures held; on again: the next
                        // frame decodes what is on screen
                        if self.gfx.name() == "none" {
                            res.release_images();
                        }
                        self.redraw_all.set(true);
                    }
                    if it.key == "sound" {
                        con.sound_on = crate::sound::wanted(&v);
                        self.setting_note = if con.sound_on { "sound on".into() } else { "sound off".into() };
                    }
                    note = std::mem::take(&mut self.setting_note);
                } else {
                    note = format!("{} takes effect the next time emuera-tty starts", it.label);
                }
            } else if rows[sel - vis.len()].1 == SEED_ROW {
                // the seed row: selectable for its description, never changed
                note = "the seed can't be changed here: set EMUERA_SEED=N before starting emuera-tty".into();
            } else if act == 1 {
                match rows[sel - vis.len()].1 {
                    0 => {
                        let quit = self.env_panel(con, res, req);
                        self.box_closed.set(true);
                        if quit {
                            break OverlayExit::Quit;
                        }
                    }
                    1 => {
                        if self.game.is_none() {
                            note = "no game folder known here".into();
                        } else {
                            let r = self.game_settings(con, res, req);
                            self.box_closed.set(true);
                            match r {
                                (_, true) => break OverlayExit::Quit,
                                (true, false) => break OverlayExit::Config,
                                _ => {}
                            }
                        }
                    }
                    2 => {
                        if self.game.is_none() {
                            note = "no game folder known here".into();
                        } else {
                            let quit = self.save_manager(con, res, req);
                            self.box_closed.set(true);
                            if quit {
                                break OverlayExit::Quit;
                            }
                        }
                    }
                    3 if st.get("translator") == "off" => note = "the Translator is off: choose one to test it".into(),
                    3 => match crate::mt::Engine::choose(st.get("translator")) {
                        Err(e) => note = e,
                        Ok(e) => {
                            // a sample in the game's language, into the one chosen
                            let from = if st.get("translate_from") == "en" { "en" } else { "ja" };
                            let sample = if from == "en" {
                                "Good morning! What would you like to do today?"
                            } else {
                                "「おはようございます。今日は何をしましょうか？」"
                            };
                            let to = st.get("translate").to_string();
                            let to = if to == from {
                                if from == "en" {
                                    "es".into()
                                } else {
                                    "en".into()
                                }
                            } else {
                                to
                            };
                            let (tx, rx) = std::sync::mpsc::channel();
                            let what = e.describe();
                            let _ = std::thread::Builder::new().name("translate-test".into()).spawn(move || {
                                let _ = tx.send(e.test(sample, from, &to));
                            });
                            note = format!("testing {}…", what);
                            test = Some((rx, Instant::now(), what));
                        }
                    },
                    4 => {
                        note = match self.mt.as_mut() {
                            Some(m) => format!("translation cache cleared ({} lines): what is on screen is translated again", m.clear()),
                            None => "translation is off: no cache to clear".into(),
                        };
                        self.force_frame = true;
                    }
                    5 => {
                        // whatever the Images setting: drop the decoded
                        // sprites, scaled copies and the like (the game's own
                        // GCREATE… buffers stay)
                        let held = |r: &Resources| r.mem_stats().1.iter().map(|g| g.2).sum::<usize>();
                        let before = held(res);
                        res.release_images();
                        note = format!("picture cache cleared ({:.1} MB freed)", before.saturating_sub(held(res)) as f64 / (1 << 20) as f64);
                        self.force_frame = true;
                    }
                    12 => {
                        let cur = st.get("recolor").to_string();
                        let what =
                            "from:to pairs of hex colours, comma separated (e.g. 01183c:111d3b turns AnonTW's time-stop navy into a softer blue; empty: none)";
                        if let Some(v) = self.prompt(con, res, req, "Recolor", what, &cur) {
                            let v = v.trim().to_string();
                            self.set_recolor(&v);
                            let n = self.recolor.len();
                            st.set("recolor", &v);
                            st.save();
                            note = if v.is_empty() {
                                "recolouring off".into()
                            } else if n == 0 {
                                format!("no colour pairs understood in \"{}\" (write from:to, e.g. 01183c:111d3b)", v)
                            } else {
                                format!("{} colour{} recoloured (the game still sees its own colours)", n, if n == 1 { "" } else { "s" })
                            };
                            self.force_frame = true;
                        }
                    }
                    6 => break OverlayExit::Title,
                    7 => break OverlayExit::Restart,
                    8 => break OverlayExit::Reload,
                    9 => break OverlayExit::SwitchGame,
                    10 => break OverlayExit::Quit,
                    _ => break OverlayExit::Close,
                }
            }
        };
        self.no_images = false;
        exit
    }

    /// Apply a changed setting to the running terminal.
    fn apply_setting(&mut self, key: &str, v: &str, st: &crate::settings::Settings) {
        match key {
            "contrast" => self.contrast = st.contrast_ratio(),
            "wheel" => self.wheel = v.parse().unwrap_or(3),
            "statusbar" => self.status_info = v == "on",
            "kbdpointer" => self.set_kbd_pointer(v),
            "translate" | "translator" => self.setting_note = self.set_translate(st),
            "translate_from" | "translate_what" => {
                self.tr_from = st.get("translate_from").to_string();
                self.tr_all = all_text(st);
                self.redraw_all.set(true);
            }
            "memory" => self.show_memory = v == "on",
            "clipboard" => self.clip_mode = v.to_string(),
            "colors" => {
                self.color = match v {
                    "truecolor" => ColorMode::True,
                    "256" => ColorMode::C256,
                    "16" => ColorMode::C16,
                    _ => self.auto_color,
                }
            }
            "wide" => {
                if v == "auto" {
                    std::env::remove_var("EMUERA_AMBIGUOUS");
                } else {
                    std::env::set_var("EMUERA_AMBIGUOUS", v);
                }
                self.ambi = self.detect_ambiguous();
            }
            "images" => {
                let mut out = Vec::new();
                self.gfx.shutdown(&mut out);
                write_all(&out);
                self.gfx = Box::new(gfx::NoGfx);
                self.fb_cell_fixed = false;
                let pref = if v == "auto" && self.gfx_pref != "auto" { self.gfx_pref.clone() } else { v.to_string() };
                let term = self.term.clone();
                self.setting_note = self.setup_gfx(&pref, &term);
            }
            _ => {}
        }
    }

    /// Translate as the settings say (Translate to / from / what,
    /// Translator). What happened, in words.
    pub fn set_translate(&mut self, st: &crate::settings::Settings) -> String {
        self.mt = None;
        self.redraw_all.set(true);
        let lang = st.get("translate");
        self.tr_to = lang.to_string();
        self.tr_from = st.get("translate_from").to_string();
        self.tr_all = all_text(st);
        if st.get("translator") == "off" {
            return "the game shows as it prints itself".into();
        }
        match crate::mt::Engine::choose(st.get("translator")) {
            Ok(e) => {
                let d = format!("translating {} into {} with {}", if self.tr_all { "the game" } else { "dialog" }, lang, e.describe());
                self.mt = Some(crate::mt::Mt::start(e, lang));
                d
            }
            Err(e) => e,
        }
    }

    /// The line's text, language and first colour if it is dialog to
    /// translate whole: only text (no buttons, pictures), not a drawing,
    /// in a language to translate, and dialog (see `mt::is_dialog`).
    fn dialog_of(&self, l: &Line) -> Option<(&'static str, String, Style)> {
        let mut text = String::new();
        let mut style = None;
        for s in &l.segs {
            match s {
                Seg::Text { text: t, style: st, button: None, .. } => {
                    text.push_str(t);
                    style.get_or_insert_with(|| st.clone());
                }
                _ => return None,
            }
        }
        if crate::mt::is_drawing(&text) || text.trim().is_empty() {
            return None;
        }
        let src = crate::mt::source_of(&text, &self.tr_from, &self.tr_to)?;
        crate::mt::is_dialog(&text, l.kojo, src).then(|| (src, text, style.unwrap()))
    }

    /// Display columns of `s`, as the screen lays it out.
    fn text_width(&self, s: &str) -> usize {
        s.chars()
            .map(|c| {
                let (tw, extra, _) = if c == '\t' { (4, 0, ' ') } else { char_cells(c, self.ambi, self.widths.get(&c).copied()) };
                tw + extra
            })
            .sum()
    }

    /// Dialog translated in batches: the lines printed between two waits or
    /// inputs (one click: a speech over several lines) that are dialog in one
    /// language go to the translator together, so sentences split over
    /// lines are translated whole. Returns the lines from `start` on that
    /// have their translation in, by index. A translation with as many
    /// lines goes back line by line; one without is spread over the lines
    /// in proportion to their lengths.
    fn translate_blocks(&mut self, con: &Console, start: usize) -> crate::fxhash::HashMap<usize, Line> {
        let mut out = crate::fxhash::HashMap::default();
        let lines = &con.lines;
        let dialog: Vec<Option<(&'static str, String, Style)>> = (start..lines.len()).map(|i| self.dialog_of(&lines[i])).collect();
        // a speech that began above `start` starts there (unchecked: rare)
        let mut i = 0;
        while i < dialog.len() {
            let Some((src, _, _)) = &dialog[i] else {
                i += 1;
                continue;
            };
            let (src, wait) = (*src, lines[start + i].wait);
            let mut j = i + 1;
            while j < dialog.len() && j - i < 40 && lines[start + j].wait == wait && dialog[j].as_ref().is_some_and(|d| d.0 == src) {
                j += 1;
            }
            let texts: Vec<&str> = (i..j).map(|k| dialog[k].as_ref().unwrap().1.trim()).collect();
            let joined = texts.join("\n");
            // where each line of the batch was printed, for the log (one row each)
            let places: Vec<String> = if self.mt.as_ref().is_some_and(|m| m.logging()) {
                (i..j).map(|k| place_of(&lines[start + k], &con.src_files).unwrap_or_else(|| "-".into())).collect()
            } else {
                Vec::new()
            };
            let tr = self.mt.as_mut().and_then(|m| m.get_lines(&joined, src, &places).map(str::to_string));
            if let Some(tr) = tr {
                let n = j - i;
                let got: Vec<&str> = tr.split('\n').collect();
                let parts: Vec<String> = if got.len() == n {
                    got.iter().map(|s| s.to_string()).collect()
                } else {
                    let widths: Vec<usize> = texts.iter().map(|t| self.text_width(t)).collect();
                    spread(&got.join(" "), &widths, |s| self.text_width(s))
                };
                for (k, part) in (i..j).zip(parts) {
                    let (_, text, style) = dialog[k].as_ref().unwrap();
                    let lead: String = text.chars().take_while(|c| c.is_whitespace()).collect();
                    let mut l = lines[start + k].clone();
                    l.segs = vec![Seg::Text { text: format!("{}{}", lead, part), style: style.clone(), button: None, auto: false }];
                    out.insert(start + k, l);
                }
            }
            i = j;
        }
        out
    }

    /// All-text mode: `l` with each piece of text translated on its own (as
    /// far as translations are in), so buttons and colours stay, and only
    /// the words, not the numbers and brackets around them. Dialog lines
    /// are left to `translate_blocks`.
    fn translated(&mut self, l: &Line, files: &[String]) -> Option<Line> {
        use crate::mt::{is_drawing, source_of, split_words};
        if !self.tr_all || self.dialog_of(l).is_some() {
            return None;
        }
        let (from, to) = (self.tr_from.clone(), self.tr_to.clone());
        let place = self.mt.as_ref().is_some_and(|m| m.logging()).then(|| place_of(l, files)).flatten();
        let place = place.as_deref();
        let mt = self.mt.as_mut()?;
        let whole: String = l.segs.iter().filter_map(|s| if let Seg::Text { text, .. } = s { Some(text.as_str()) } else { None }).collect();
        if is_drawing(&whole) {
            return None;
        }
        {
            // menus keep their columns: where a piece is followed by a gap
            // (two spaces or more, then more on the line) its translation
            // keeps its width — padded if shorter, taking the gap's spaces
            // (one stays) if longer, cut with … past that. At the end of a
            // line, or before ordinary text, it just takes the room it needs.
            let (ambi, widths) = (self.ambi, self.widths.clone());
            let width = |s: &str| -> usize {
                s.chars()
                    .map(|c| {
                        let (tw, extra, _) = if c == '\t' { (4, 0, ' ') } else { char_cells(c, ambi, widths.get(&c).copied()) };
                        tw + extra
                    })
                    .sum()
            };
            let lead_spaces = |s: &str| s.len() - s.trim_start_matches(' ').len();
            let mut out = l.clone();
            let mut changed = false;
            // spaces a piece took from the ones after it
            let mut eat = 0usize;
            for k in 0..out.segs.len() {
                // the spaces at the start of the pieces after this one, and
                // whether anything comes after them
                let (mut next_lead, mut more) = (0usize, false);
                for s in &out.segs[k + 1..] {
                    match s {
                        Seg::Text { text, .. } if text.trim_start_matches(' ').is_empty() => next_lead += text.len(),
                        Seg::Text { text, .. } => {
                            next_lead += lead_spaces(text);
                            more = true;
                            break;
                        }
                        _ => {
                            more = true;
                            break;
                        }
                    }
                }
                let Seg::Text { text, .. } = &mut out.segs[k] else {
                    eat = 0;
                    continue;
                };
                if eat > 0 {
                    let n = eat.min(lead_spaces(text));
                    text.drain(..n);
                    changed = true;
                    eat -= n;
                    if !text.is_empty() {
                        eat = 0;
                    }
                }
                let (pre, words, post) = split_words(text);
                let Some(src) = source_of(words, &from, &to) else { continue };
                let Some(tr) = mt.get(words, src, place) else { continue };
                let (want, got) = (width(words), width(tr));
                let mut post = post.to_string();
                let tr = if got <= want {
                    post.push_str(&" ".repeat(want - got));
                    tr.to_string()
                } else {
                    // blank space after it: its own trailing spaces, then the
                    // next piece's leading ones, keeping one
                    let own = post.len() - post.trim_end_matches(' ').len();
                    let gap = own + next_lead;
                    let slack = gap.saturating_sub(1);
                    let over = got - want;
                    // not a column: nothing after it, or ordinary text
                    let in_column = more && gap >= 2;
                    if !in_column || over <= slack {
                        let over = if in_column { over } else { over.min(slack) };
                        let from_own = over.min(own);
                        post.truncate(post.len() - from_own);
                        eat = over - from_own;
                        tr.to_string()
                    } else {
                        post.truncate(post.len() - own.min(slack));
                        eat = slack - own.min(slack);
                        let room = want + slack;
                        let mut cut = String::new();
                        for c in tr.chars() {
                            if width(&cut) + width(&c.to_string()) + 1 > room {
                                break;
                            }
                            cut.push(c);
                        }
                        cut.push('…');
                        cut
                    }
                };
                *text = format!("{}{}{}", pre, tr, post);
                changed = true;
            }
            changed.then_some(out)
        }
    }

    /// The SGR sequence for an overlay colour pair, in the terminal's depth.
    fn ov_style(&self, fg: (u8, u8, u8), bg: (u8, u8, u8), bold: bool) -> String {
        let b = if bold { "1;" } else { "" };
        match self.color {
            ColorMode::True => format!("\x1b[0;{}38;2;{};{};{};48;2;{};{};{}m", b, fg.0, fg.1, fg.2, bg.0, bg.1, bg.2),
            ColorMode::C256 if self.fbterm => {
                format!("\x1b[0;{}m\x1b[1;{}}}\x1b[2;{}}}", b.trim_end_matches(';'), rgb256(Rgb(fg.0, fg.1, fg.2)), rgb256(Rgb(bg.0, bg.1, bg.2)))
            }
            ColorMode::C256 => format!("\x1b[0;{}38;5;{};48;5;{}m", b, rgb256(Rgb(fg.0, fg.1, fg.2)), rgb256(Rgb(bg.0, bg.1, bg.2))),
            // 16 colours: white / yellow on black, black on white
            ColorMode::C16 => {
                let f = if fg == (0, 0, 0) {
                    30
                } else if fg.2 < 100 {
                    93
                } else {
                    97
                };
                let k = if bg == (0, 0, 0) || bg.0 < 128 { 40 } else { 47 };
                format!("\x1b[0;{}{};{}m", b, f, k)
            }
        }
    }

    /// The foreground-only SGR for a memory band, in the terminal's depth
    /// (the background and the other attributes stay as they are).
    fn mem_fg(&self, lvl: crate::sys::MemoryLevel) -> String {
        use crate::sys::MemoryLevel::{Green, Orange, Red, Yellow};
        let c = match lvl {
            Green => Rgb(90, 200, 90),
            Yellow => Rgb(240, 220, 70),
            Orange => Rgb(255, 150, 40),
            Red => Rgb(240, 70, 70),
        };
        match self.color {
            ColorMode::True => format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2),
            ColorMode::C256 if self.fbterm => format!("\x1b[1;{}}}", rgb256(c)),
            ColorMode::C256 => format!("\x1b[38;5;{}m", rgb256(c)),
            // 16 colours: orange is the plain (brownish) yellow and yellow the
            // bright one; the Linux console and fbterm brighten via bold
            ColorMode::C16 => {
                let code = match lvl {
                    Green => "\x1b[32m",
                    Yellow => "\x1b[33;1m",
                    Orange => "\x1b[33m",
                    Red => "\x1b[31m",
                };
                code.to_string()
            }
        }
    }

    /// Draw the overlay box; returns its top row (1-based, the title row)
    /// for mapping clicks.
    fn draw_overlay(&self, st: &crate::settings::Settings, vis: &[usize], extra: &[&str], sel: usize, note: &str) -> (usize,) {
        self.redraw_all.set(true);
        use crate::settings::ITEMS;
        let n = vis.len() + extra.len();
        let w = self.cols.saturating_sub(4).clamp(30, 72);
        let help = if !note.is_empty() {
            note.to_string()
        } else if let (true, Some(e)) = (
            sel < vis.len() && matches!(ITEMS[vis[sel]].key, "translate" | "translator" | "translate_from" | "translate_what"),
            self.mt.as_ref().and_then(|m| m.last_error.as_ref()),
        ) {
            format!("last translation failed: {}", e)
        } else if sel < vis.len() {
            ITEMS[vis[sel]].help.to_string()
        } else if sel == vis.len() {
            "the number the random generator started from (read-only); set EMUERA_SEED=N before starting for repeatable runs; RANDOMIZE / INITRAND in the game change it".to_string()
        } else {
            "↑↓ choose  ←→ / Enter change  Esc close".to_string()
        };
        // the help wraps (three lines at most) rather than being cut
        // (always three lines high, so the box doesn't move between rows)
        let mut help = wrap_words(&help, w.saturating_sub(3), 3);
        help.resize(3, String::new());
        let h = n + 3 + help.len(); // title, rows, help, borders
        let left = (self.cols.saturating_sub(w)) / 2 + 1;
        let top = (self.rows.saturating_sub(h)) / 2 + 1;
        let mut out: Vec<u8> = Vec::new();
        let inner = w - 2;
        // fixed colours where the terminal has them: palette colours vary
        // by theme (Konsole's blue is bright, and bright cyan on it vanishes)
        let rgb = |fg: (u8, u8, u8), bg: (u8, u8, u8), bold: bool| -> String { self.ov_style(fg, bg, bold) };
        let box_bg = (22, 32, 58);
        let frame = rgb((150, 170, 210), box_bg, false);
        let title = rgb((255, 255, 255), box_bg, true);
        let normal = rgb((235, 235, 235), box_bg, false);
        let action = rgb((140, 220, 255), box_bg, false);
        let hilite = rgb((0, 0, 0), (230, 230, 230), false);
        let helpc = rgb((255, 220, 90), box_bg, false);
        let line = |out: &mut Vec<u8>, row: usize, text: &str, style: &str| {
            let mut t: String = String::new();
            let mut used = 0;
            for c in text.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw > inner {
                    break;
                }
                t.push(c);
                used += cw;
            }
            let _ = write!(out, "\x1b[{};{}H{}│{}{}{}{}│", row, left, frame, style, t, " ".repeat(inner - used), frame);
        };
        let _ = write!(out, "\x1b[?25l\x1b[{};{}H{}┌{}┐", top, left, frame, "─".repeat(inner));
        // the memory emuera-tty uses, on the right of the title
        let head = " emuera-tty settings  (F1 / Ctrl-O)";
        let reading = crate::sys::memory_in_use();
        let mem = reading.map(|b| format!("memory {} ", crate::sys::memory_text(b))).unwrap_or_default();
        let head_cols = head.chars().count();
        let gap = inner.saturating_sub(head_cols + mem.len());
        let head = if gap > 0 { format!("{}{}{}", head, " ".repeat(gap), mem) } else { head.to_string() };
        line(&mut out, top + 1, &head, &title);
        // only the live counter is coloured (not the peak after it): drawn
        // over its place in the title row, after the box's left border
        if let (true, Some(b)) = (gap > 0, reading) {
            let (live, _) = crate::sys::memory_parts(b);
            let col = left + 1 + head_cols + gap + "memory ".len();
            let _ = write!(out, "\x1b[{};{}H{}{}{}", top + 1, col, title, self.mem_fg(crate::sys::memory_level(b)), live);
        }
        for (i, &ix) in vis.iter().enumerate() {
            let it = &ITEMS[ix];
            let later = if it.live { "" } else { "  (next start)" };
            let text = format!("  {:<18} ‹ {} ›{}", it.label, st.get(it.key), later);
            line(&mut out, top + 2 + i, &text, if i == sel { &hilite } else { &normal });
        }
        for (k, e) in extra.iter().enumerate() {
            let i = vis.len() + k;
            line(&mut out, top + 2 + i, &format!("  {}", e), if i == sel { &hilite } else { &action });
        }
        for (k, l) in help.iter().enumerate() {
            line(&mut out, top + 2 + n + k, &format!(" {}", l), &helpc);
        }
        let _ = write!(out, "\x1b[{};{}H{}└{}┘\x1b[0m", top + 2 + n + help.len(), left, frame, "─".repeat(inner));
        write_all(&out);
        (top + 1,)
    }

    /// The right side of the bottom row: the value the hovered button
    /// sends, memory in use, the function asking for input, the settings
    /// key. What doesn't fit goes: the function, memory, the key, the value
    /// last. (Redrawn on its own when the hovered button changes.)
    fn draw_bar(&mut self, out: &mut Vec<u8>, hover: Option<usize>) {
        let Some((used, running, drawn_at)) = self.bar.clone() else { return };
        let cols = self.cols;
        let mut parts: Vec<(String, &str)> = Vec::new();
        // the memory part's text and band, to colour its live counter
        let mut mem_mark: Option<(String, crate::sys::MemoryLevel)> = None;
        if self.status_info {
            if let Some(v) = hover.filter(|&b| self.is_button(b)).and_then(|b| self.buttons.get(b)) {
                parts.push((format!("→ {}", printable(&v.to_string())), "\x1b[0;1m"));
            }
            if let Some(m) = self.memory().filter(|_| self.show_memory) {
                mem_mark = self.mem_level().map(|l| (m.clone(), l));
                parts.push((m, "\x1b[0;2m"));
            }
            if !running.is_empty() {
                parts.push((format!("@{}", running), "\x1b[0;2m"));
            }
            // on the Linux console: is the mouse there, and are its events
            // coming in?
            if self.term == "linux" || self.term.starts_with("fbterm") {
                let m = match &self.mice {
                    Some(m) => format!("mouse:{} {}", m.describe().split(' ').next().unwrap_or(""), self.mouse_events),
                    None => "mouse:none".to_string(),
                };
                parts.insert(0, (m, "\x1b[0;1m"));
            }
        }
        parts.push(("F1 settings".to_string(), "\x1b[0;2m"));
        let width = |p: &[(String, &str)]| p.iter().map(|(s, _)| s.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(0)).sum::<usize>() + 2).sum::<usize>();
        while !parts.is_empty() && used + width(&parts) >= cols {
            let k = parts
                .iter()
                .position(|(s, _)| s.starts_with('@'))
                .or_else(|| parts.iter().position(|(s, _)| s.ends_with(" MB")))
                .or_else(|| parts.iter().position(|(s, _)| s == "F1 settings"))
                .unwrap_or(0);
            parts.remove(k);
        }
        // what was there before goes first
        if drawn_at > 0 {
            let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", self.rows, drawn_at);
        }
        let mut at = 0;
        if !parts.is_empty() {
            // (ending a column short of the edge: no wrap on the last row)
            at = cols + 2 - width(&parts);
            let _ = write!(out, "\x1b[{};{}H", self.rows, at);
            for (k, (s, st)) in parts.iter().enumerate() {
                let sep = if k > 0 { "  " } else { "" };
                match mem_mark.as_ref().filter(|(m, _)| m == s) {
                    // only the live counter is coloured, not the peak after it
                    Some((_, lvl)) => {
                        let cut = s.find(" MB").map_or(s.len(), |i| i + 3);
                        let _ = write!(out, "{}\x1b[0m{}{}{}{}\x1b[0m", sep, self.mem_fg(*lvl), &s[..cut], st, &s[cut..]);
                    }
                    None => {
                        let _ = write!(out, "{}{}{}\x1b[0m", sep, st, s);
                    }
                }
            }
        }
        if let Some(b) = self.bar.as_mut() {
            b.2 = at;
        }
    }

    /// "123 MB": memory in use (read at most once a second).
    fn memory(&mut self) -> Option<String> {
        let fresh = self.mem_seen.is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(1));
        if !fresh {
            self.mem_seen = Some((Instant::now(), crate::sys::memory_in_use()));
        }
        self.mem_seen.and_then(|(_, m)| m).map(crate::sys::memory_text)
    }

    /// The colour band of the reading [`Self::memory`] shows.
    fn mem_level(&self) -> Option<crate::sys::MemoryLevel> {
        self.mem_seen.and_then(|(_, m)| m).map(crate::sys::memory_level)
    }

    /// The button id at a 0-based screen cell, from the last frame
    /// (`<nonbutton>` tooltip areas included).
    fn button_at(&self, x: usize, y: usize) -> Option<usize> {
        let row = self.btn_map.get(y)?;
        row.iter().find(|(s, e, _)| x >= *s && x < *e).map(|(_, _, b)| *b)
    }

    /// The Keyboard pointer setting: on, off, or auto (on on the Linux
    /// console when there is no mouse there).
    /// A colour as drawn (see `recolor`).
    fn recolored(&self, c: Rgb) -> Rgb {
        self.recolor.iter().find(|(from, _)| *from == c).map_or(c, |(_, to)| *to)
    }

    /// The `recolor` setting (or EMUERA_RECOLOR).
    pub fn set_recolor(&mut self, v: &str) {
        self.recolor = parse_recolor(v);
    }

    pub fn set_kbd_pointer(&mut self, v: &str) {
        let console = self.term == "linux" || self.term.starts_with("fbterm");
        self.kbd_ptr = v == "on" || (v == "auto" && console && self.mice.is_none());
        if self.kbd_ptr && self.mouse.is_none() {
            self.mouse = Some((self.cols / 2, self.rows / 2));
        }
        self.redraw_all.set(true);
    }

    /// Is the mouse pointer ours to draw? (the console mouse, gpm's or the
    /// raw device's, or the keyboard pointer: no terminal shows one)
    fn own_pointer(&self) -> bool {
        self.mice.is_some() || self.kbd_ptr
    }

    /// Blink the pointer: how long until it changes (None: no pointer of
    /// ours), turning it over when the time has come. It shows at once when
    /// it moves (`show_pointer`).
    fn blink(&mut self) -> Option<i32> {
        if !self.own_pointer() || self.mouse.is_none() {
            return None;
        }
        let now = Instant::now();
        if now >= self.blink_at {
            self.ptr_on = !self.ptr_on;
            self.blink_at = now + Duration::from_millis(530);
        }
        Some(self.blink_at.saturating_duration_since(now).as_millis() as i32 + 1)
    }

    /// Blink the pointer when it is time (repainting just its cell); how
    /// long to wait for the next time.
    fn blink_wait(&mut self, focus: Rgb) -> Option<i32> {
        let was = self.ptr_on;
        let w = self.blink();
        if self.ptr_on != was {
            self.repaint_hover(focus);
        }
        w
    }

    /// A line in the mouse log (EMUERA_MOUSE_LOG), with the time.
    fn mlog(&mut self, what: &str) {
        if let Some(f) = self.mouse_log.as_mut() {
            use std::io::Write;
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() % 1000.0);
            let _ = writeln!(f, "{:8.3} {}", t, what);
        }
    }

    /// The pointer moved: show it, and keep it on for a moment.
    fn show_pointer(&mut self) {
        self.ptr_on = true;
        self.blink_at = Instant::now() + Duration::from_millis(530);
    }

    /// The keyboard pointer: with it on, the cursor keys move the pointer a
    /// cell and Insert or Shift+Enter click under it, as mouse events (Enter
    /// keeps its own uses). `raw`: a picture menu's input.
    fn kbd_pointer(&mut self, k: Key, _raw: bool) -> Key {
        if !self.kbd_ptr {
            return k;
        }
        let (x, y) = self.mouse.unwrap_or((self.cols / 2, self.rows / 2));
        let (dx, dy): (isize, isize) = match k {
            Key::Up => (0, -1),
            Key::Down => (0, 1),
            Key::Left => (-1, 0),
            Key::Right => (1, 0),
            Key::Click => return Key::Mouse { btn: 0, x: x as u16 + 1, y: y as u16 + 1, press: true },
            k => return k,
        };
        let nx = (x as isize + dx).clamp(0, self.cols.saturating_sub(1) as isize) as usize;
        let ny = (y as isize + dy).clamp(0, self.rows.saturating_sub(2) as isize) as usize;
        // a move with no button held (SGR motion)
        Key::Mouse { btn: 35, x: nx as u16 + 1, y: ny as u16 + 1, press: true }
    }

    /// The clickable button at a cell (not a `<nonbutton>` area).
    fn click_at(&self, x: usize, y: usize) -> Option<usize> {
        self.button_at(x, y).filter(|&b| self.is_button(b))
    }

    /// Is button id `b` a real button (not a `<nonbutton>` tooltip area)?
    fn is_button(&self, b: usize) -> bool {
        self.buttons.get(b).is_some_and(|v| !crate::console::is_tip_area(v))
    }
}

/// Nearest xterm-256 colour: the 6×6×6 cube or, for greys, the 24-step
/// grey ramp (the cube has only black below 95, so dark map tiles and
/// shading would all turn black).
fn rgb256(c: Rgb) -> u8 {
    let q = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let lv = |k: u8| -> i32 {
        if k == 0 {
            0
        } else {
            55 + 40 * k as i32
        }
    };
    let (r, g, b) = (c.0 as i32, c.1 as i32, c.2 as i32);
    let cube = (q(c.0), q(c.1), q(c.2));
    let d_cube = (lv(cube.0) - r).pow(2) + (lv(cube.1) - g).pow(2) + (lv(cube.2) - b).pow(2);
    // grey ramp 232..=255 is 8, 18, ..., 238
    let avg = (r + g + b) / 3;
    let gi = ((avg - 3) / 10).clamp(0, 23);
    let gv = 8 + 10 * gi;
    let d_grey = (gv - r).pow(2) + (gv - g).pow(2) + (gv - b).pow(2);
    if d_grey < d_cube && avg >= 4 {
        232 + gi as u8
    } else {
        16 + 36 * cube.0 + 6 * cube.1 + cube.2
    }
}

/// One of the 8 ANSI colours for an RGB colour, and whether it should be
/// the bright one. Hues are kept even when dark (Emuera games draw dim
/// teal walls or brown floors on black, which nearest-distance would turn
/// into black); greys step through black, dark grey, light grey, white.
pub(crate) fn rgb16(c: Rgb) -> (u8, bool) {
    let (r, g, b) = (c.0 as u32, c.1 as u32, c.2 as u32);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max < 24 {
        return (0, false);
    }
    // greyish: little colour compared with the brightness
    if (max - min) * 4 < max {
        return match max {
            0..=39 => (0, false),
            40..=127 => (0, true),
            128..=210 => (7, false),
            _ => (7, true),
        };
    }
    // which channels stand out within the colour's own range
    let t = min + (max - min) / 3;
    let code = ((r > t) as u8) | ((g > t) as u8) << 1 | ((b > t) as u8) << 2;
    (code, max > 200)
}

/// An ANSI background colour for a window background: only the strongest
/// channels count, so a dark navy stays blue (not cyan) and very dark
/// colours stay black.
fn rgb16_bg(c: Rgb) -> u8 {
    let (r, g, b) = (c.0 as u32, c.1 as u32, c.2 as u32);
    let max = r.max(g).max(b);
    if max < 40 {
        return 0;
    }
    let t = max * 2 / 3;
    ((r > t) as u8) | ((g > t) as u8) << 1 | ((b > t) as u8) << 2
}

#[cfg(test)]
mod colour_tests {
    use super::*;

    fn row(text: &str) -> DRow {
        let st = Style { fg: Rgb(0, 0, 0), bg: None, bold: false, italic: false, underline: false, strike: false };
        DRow {
            cells: text.chars().map(|ch| Cell { ch, w: if ch.is_ascii() { 1 } else { 2 }, style: st.clone(), button: None }).collect(),
            ..Default::default()
        }
    }

    fn text(r: &DRow) -> String {
        r.cells.iter().map(|c| c.ch).collect()
    }

    #[test]
    fn div_cells_paint_over_rows() {
        let mut r = row("abcdef");
        paint_cells(&mut r, 2, row("XY").cells);
        assert_eq!(text(&r), "abXYef");
        // past the end: padded
        let mut r = row("ab");
        paint_cells(&mut r, 4, row("Z").cells);
        assert_eq!(text(&r), "ab  Z");
        // a wide character cut in half leaves a blank
        let mut r = row("a漢b");
        paint_cells(&mut r, 2, row("Q").cells);
        assert_eq!(text(&r), "a Qb");
    }

    #[test]
    fn dark_tints_stay_visible() {
        // dim teal / brown map tiles keep their hue
        assert_eq!(rgb16(Rgb(0x01, 0x3d, 0x3d)), (6, false));
        assert_eq!(rgb16(Rgb(0x47, 0x29, 0x1f)), (1, false));
        assert_eq!(rgb16(Rgb(0xc9, 0xa1, 0x79)), (3, true));
        assert_eq!(rgb16(Rgb(0xff, 0x66, 0xcc)), (5, true));
        assert_eq!(rgb16(Rgb(0x50, 0xa0, 0xf0)), (6, true));
        // the grey vignette fades out through dark grey to black
        assert_eq!(rgb16(Rgb(0x79, 0x79, 0x79)), (0, true));
        assert_eq!(rgb16(Rgb(0x29, 0x29, 0x29)), (0, true));
        assert_eq!(rgb16(Rgb(0x0b, 0x0b, 0x0b)), (0, false));
        assert_eq!(rgb16(Rgb(0xc0, 0xc0, 0xc0)), (7, false));
        assert_eq!(rgb16(Rgb(0xff, 0xff, 0xff)), (7, true));
        assert_eq!(rgb16(Rgb(0xff, 0xff, 0x00)), (3, true));
        // 256 colours: dark greys use the grey ramp, not cube black
        assert_eq!(rgb256(Rgb(0x15, 0x15, 0x15)), 233);
        assert_eq!(rgb256(Rgb(0xff, 0x00, 0x00)), 196);
        assert_eq!(rgb256(Rgb(0, 0, 0)), 16);
    }
}

impl Frontend for Tty {
    fn flush(&mut self, con: &mut Console, res: &mut Resources) {
        if con.redraw {
            self.render(con, res, None);
        }
    }

    fn refresh(&mut self, con: &mut Console, res: &mut Resources) {
        self.render(con, res, None);
    }

    /// Show the console and wait for input: typed text + Enter, a button
    /// click, any key/click for WAIT, or a timeout. Scrolling keys and the
    /// wheel move through the log meanwhile.
    fn input(&mut self, con: &mut Console, res: &mut Resources, req: &InputReq) -> InputResult {
        if !std::mem::take(&mut self.keep_buf) || req.kind == InputKind::Wait {
            self.buf.clear();
        }
        self.scroll = 0;
        self.render(con, res, Some(req));
        self.copy_text(con, if req.kind == InputKind::Wait { crate::clipboard::Trigger::Wait } else { crate::clipboard::Trigger::Input });
        let deadline = req.timeout_ms.map(|t| Instant::now() + Duration::from_millis(t));
        let (mut last_cols, mut last_rows) = (self.cols, self.rows);
        // a redraw owed for input already handled: done once the burst of
        // input (mouse motion, a held key) has been read, not per event
        let mut owed = false;
        // only the console pointer moved: repaint just its cells
        let mut pointer_owed = false;
        loop {
            if self.hangup {
                return InputResult::Quit;
            }
            if (owed || pointer_owed) && self.queued.is_empty() && self.pending.is_empty() && !self.input_waiting() {
                if owed || self.hover_swaps_image() {
                    self.render(con, res, Some(req));
                } else {
                    self.repaint_hover(con.focus_fg);
                }
                owed = false;
                pointer_owed = false;
            }
            let wait = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        self.keep_buf = true;
                        return InputResult::Timeout;
                    }
                    ((d - now).as_millis() as i32).min(200)
                }
                None => 200,
            };
            // SETANIMETIMER: redraw animated sprites that often (at most
            // 10 times a second, terminals are slower than a window)
            let anim = self.animating && res.anim_timer > 0;
            let wait = if anim { wait.min(res.anim_timer.max(100) as i32) } else { wait };
            // a tooltip's delay or duration running out
            let tip_next = self.tip_due(con).1;
            let wait = match tip_next {
                Some(t) => wait.min(t.saturating_duration_since(Instant::now()).as_millis() as i32 + 1),
                None => wait,
            };
            // dialog being translated: look for the results now and then
            let wait = if self.mt.as_ref().is_some_and(|m| m.busy()) { wait.min(250) } else { wait };
            // the pointer blinking
            let wait = self.blink_wait(con.focus_fg).map_or(wait, |b| wait.min(b));
            let Some(k) = self.next_key(wait).map(|k| self.kbd_pointer(k, false)) else {
                let (c, r, _, _) = winsize();
                let translated = self.mt.as_mut().is_some_and(|m| m.poll());
                if (c, r) != (last_cols, last_rows) || anim || translated || self.tip_due(con).0 != self.tip_drawn {
                    last_cols = c;
                    last_rows = r;
                    self.render(con, res, Some(req));
                }
                continue;
            };
            let mut redraw = false;
            match k {
                Key::CtrlC => return InputResult::Quit,
                Key::CtrlL => {
                    self.force_frame = true;
                    redraw = true;
                }
                Key::Title | Key::Restart | Key::Reload => {
                    if let Some(r) = self.anchor_hotkey(con, res, req, &k) {
                        return r;
                    }
                    redraw = true;
                }
                Key::Settings => match self.settings_overlay(con, res, req) {
                    OverlayExit::Close => redraw = true,
                    OverlayExit::Title => {
                        if let Some(r) = self.anchor_hotkey(con, res, req, &Key::Title) {
                            return r;
                        }
                        redraw = true;
                    }
                    OverlayExit::Restart => {
                        if let Some(r) = self.anchor_hotkey(con, res, req, &Key::Restart) {
                            return r;
                        }
                        redraw = true;
                    }
                    OverlayExit::Reload => return InputResult::Hotkey(crate::frontend::Hotkey::Reload),
                    OverlayExit::Config => return InputResult::Hotkey(crate::frontend::Hotkey::Config),
                    OverlayExit::Quit => return InputResult::Quit,
                    OverlayExit::SwitchGame => {
                        // start over on the launcher (this game's unsaved
                        // progress goes, as the overlay warns)
                        self.shutdown();
                        let e = crate::sys::exec_self(&["--choose".into()]);
                        eprintln!("emuera-tty: can't restart: {}", e);
                        return InputResult::Quit;
                    }
                },
                Key::PageUp | Key::Up if req.kind == InputKind::Wait || self.buf.is_empty() => {
                    self.scroll += if matches!(k, Key::PageUp) { self.rows / 2 } else { 1 };
                    redraw = true;
                }
                Key::PageDown | Key::Down => {
                    self.scroll = self.scroll.saturating_sub(if matches!(k, Key::PageDown) { self.rows / 2 } else { 1 });
                    redraw = true;
                }
                Key::Home => {
                    self.scroll = usize::MAX / 2;
                    redraw = true;
                }
                Key::End => {
                    self.scroll = 0;
                    redraw = true;
                }
                Key::Mouse { btn, x, y, press } => {
                    if press {
                        self.click_copies(con, btn, x, y);
                    }
                    let (x, y) = (x.saturating_sub(1) as usize, y.saturating_sub(1) as usize);
                    // ignore Shift / Alt / Ctrl so modified clicks still work
                    match btn & !(4 | 8 | 16) {
                        64 => {
                            self.scroll += self.wheel;
                            redraw = true;
                        }
                        65 => {
                            self.scroll = self.scroll.saturating_sub(self.wheel);
                            redraw = true;
                        }
                        b if b & 32 != 0 => {
                            // motion: update hover highlight
                            self.mouse = Some((x, y));
                            self.show_pointer();
                            // the console pointer is drawn by us: move it, but
                            // only once a burst of motion has been read
                            if self.own_pointer() || self.button_at(x, y) != self.hover {
                                pointer_owed = true;
                            }
                        }
                        0 if press => {
                            if req.kind == InputKind::Wait {
                                return InputResult::Text(String::new());
                            }
                            if let Some(b) = self.click_at(x, y) {
                                let v = self.buttons[b].clone();
                                return InputResult::Text(v.to_string());
                            }
                        }
                        // a middle click on a translated line flips it
                        // between the original and the translation (also at
                        // a WAIT)
                        1 if press => redraw |= self.toggle_translation(y),
                        // a right click: as Emuera, continue and skip the
                        // following waits up to the next choice; on a
                        // button, choose it and skip after it
                        2 if press => {
                            if req.kind == InputKind::Wait {
                                self.scroll = 0;
                                return InputResult::Skip(String::new());
                            }
                            if let Some(b) = self.click_at(x, y) {
                                let v = self.buttons[b].clone();
                                return InputResult::Skip(v.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                Key::Skip if req.kind == InputKind::Wait => {
                    self.scroll = 0;
                    return InputResult::Skip(String::new());
                }
                _ if req.kind == InputKind::Wait => {
                    if !matches!(k, Key::Other | Key::Left | Key::Right) {
                        return InputResult::Text(String::new());
                    }
                }
                Key::Enter => {
                    let s = std::mem::take(&mut self.buf);
                    return InputResult::Text(s);
                }
                Key::Backspace => {
                    self.buf.pop();
                    redraw = true;
                }
                Key::Esc => {
                    self.buf.clear();
                    redraw = true;
                }
                Key::Char('\u{15}') => {
                    self.buf.clear();
                    redraw = true;
                }
                Key::Char(c) => {
                    if req.one {
                        return InputResult::Text(c.to_string());
                    }
                    self.buf.push(c);
                    redraw = true;
                }
                _ => {}
            }
            owed |= redraw;
        }
    }

    fn mouse_cell(&self) -> Option<(usize, usize)> {
        self.mouse
    }

    fn await_input(&mut self, con: &mut Console, res: &mut Resources, ms: u64) -> bool {
        use crate::frontend::char_keycode;
        // the input box shows what is typed, as at an INPUTS
        let req = InputReq { kind: InputKind::Str, one: false, timeout_ms: None, stop_skip: false };
        self.render(con, res, Some(&req));
        let end = Instant::now() + Duration::from_millis(ms);
        let mut changed = false;
        loop {
            if self.hangup {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
            let Some(k) = self.next_key(left).map(|k| self.kbd_pointer(k, false)) else {
                if Instant::now() >= end {
                    break;
                }
                continue;
            };
            let hit = |t: &mut Tty, code: i64| {
                t.keys_hit.insert(code);
            };
            match k {
                Key::CtrlC => return true,
                Key::Char(c) => {
                    self.buf.push(c);
                    hit(self, char_keycode(c).0);
                    changed = true;
                }
                Key::Backspace => {
                    self.buf.pop();
                    hit(self, 8);
                    changed = true;
                }
                Key::Enter => hit(self, 13),
                Key::Esc => hit(self, 27),
                Key::Left => hit(self, 37),
                Key::Up => hit(self, 38),
                Key::Right => hit(self, 39),
                Key::Down => hit(self, 40),
                Key::Mouse { btn, x, y, press } => {
                    let (x, y) = (x.saturating_sub(1) as usize, y.saturating_sub(1) as usize);
                    self.mouse = Some((x, y));
                    // Windows codes: 1 left, 2 right, 4 middle
                    let code = match btn & !(4 | 8 | 16 | 32) {
                        0 => 1,
                        2 => 2,
                        1 => 4,
                        _ => 0,
                    };
                    if btn & 32 != 0 {
                        self.show_pointer();
                        self.repaint_hover(con.focus_fg);
                    } else if code != 0 && press {
                        self.keys_down.insert(code);
                        hit(self, code);
                    } else if code != 0 {
                        self.keys_down.remove(&code);
                    }
                }
                _ => {}
            }
        }
        if changed {
            self.render(con, res, Some(&req));
        }
        false
    }

    fn textbox(&self) -> String {
        self.buf.clone()
    }

    fn set_textbox(&mut self, s: &str) {
        self.buf = s.to_string();
    }

    fn hover_value(&self) -> String {
        match self.hover {
            Some(b) if self.is_button(b) => self.buttons[b].as_string(),
            _ => String::new(),
        }
    }

    fn key_state(&mut self, code: i64, triggered: bool) -> bool {
        if triggered {
            self.keys_hit.remove(&code)
        } else {
            // a terminal reports key presses, not releases: a key counts as
            // held until it is read once
            self.keys_down.contains(&code) || self.keys_hit.contains(&code)
        }
    }

    /// INPUTMOUSEKEY: the next click, wheel step or key (mouse moves only
    /// update the pointer, which MOUSEX / MOUSEY read), or the timeout.
    fn input_raw(&mut self, con: &mut Console, res: &mut Resources, timeout_ms: Option<u64>) -> crate::frontend::RawInput {
        use crate::frontend::{char_keycode, RawInput};
        self.scroll = 0;
        self.buf.clear();
        self.render(con, res, None);
        let deadline = timeout_ms.map(|t| Instant::now() + Duration::from_millis(t));
        let mut moved = false;
        let cbg_buttons = res.cbg.iter().any(|l| l.button.is_some());
        loop {
            if self.hangup {
                return RawInput::Quit;
            }
            if moved && self.queued.is_empty() && self.pending.is_empty() && !self.input_waiting() {
                // a picture button may light up under the pointer
                if cbg_buttons || self.hover_swaps_image() {
                    self.render(con, res, None);
                } else {
                    self.repaint_hover(con.focus_fg);
                }
                moved = false;
            }
            let wait = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return RawInput::Timeout;
                    }
                    ((d - now).as_millis() as i32).clamp(1, 200)
                }
                None => 200,
            };
            let wait = self.blink_wait(con.focus_fg).map_or(wait, |b| wait.min(b));
            let Some(k) = self.next_key(wait).map(|k| self.kbd_pointer(k, true)) else { continue };
            let key = |code: i64| RawInput::Key { code, data: code };
            match k {
                Key::CtrlC => return RawInput::Quit,
                Key::Title | Key::Restart | Key::Reload => {
                    let req = InputReq { kind: InputKind::Wait, one: false, timeout_ms: None, stop_skip: false };
                    let r = self.anchor_hotkey(con, res, &req, &k);
                    self.force_frame = true;
                    self.render(con, res, None);
                    match r {
                        Some(InputResult::Hotkey(h)) => return RawInput::Hotkey(h),
                        Some(_) => return RawInput::Quit,
                        None => {}
                    }
                }
                Key::Settings => match self.settings_overlay(con, res, &InputReq { kind: InputKind::Wait, one: false, timeout_ms: None, stop_skip: false }) {
                    OverlayExit::Quit => return RawInput::Quit,
                    OverlayExit::Config => return RawInput::Hotkey(crate::frontend::Hotkey::Config),
                    _ => {
                        self.force_frame = true;
                        self.render(con, res, None);
                    }
                },
                Key::CtrlL => {
                    self.force_frame = true;
                    self.render(con, res, None);
                }
                Key::Mouse { btn, x, y, press } => {
                    if press {
                        self.click_copies(con, btn, x, y);
                    }
                    let cell = (x.saturating_sub(1) as usize, y.saturating_sub(1) as usize);
                    match btn & !(4 | 8 | 16) {
                        64 => return RawInput::Wheel { up: true, cell },
                        65 => return RawInput::Wheel { up: false, cell },
                        b if b & 32 != 0 => {
                            self.mouse = Some(cell);
                            self.show_pointer();
                            moved = true;
                        }
                        b @ 0..=2 if press => {
                            self.mouse = Some(cell);
                            let button = self.click_at(cell.0, cell.1).map(|i| self.buttons[i].clone());
                            return RawInput::Click { btn: b as u8, cell, button };
                        }
                        _ => {}
                    }
                }
                // Windows key codes (System.Windows.Forms.Keys)
                Key::Enter => return key(13),
                Key::Backspace => return key(8),
                Key::Esc => return key(27),
                Key::PageUp => return key(33),
                Key::PageDown => return key(34),
                Key::End => return key(35),
                Key::Home => return key(36),
                Key::Left => return key(37),
                Key::Up => return key(38),
                Key::Right => return key(39),
                Key::Down => return key(40),
                Key::Click => return key(45),
                Key::Skip => return key(9),
                Key::Char(c) => {
                    let (code, data) = char_keycode(c);
                    return RawInput::Key { code, data };
                }
                Key::Other => {}
            }
        }
    }

    fn cols(&self) -> usize {
        self.cols
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    /// Ctrl+C typed (or the terminal gone) while a script runs. Other keys
    /// read meanwhile stay pending for the next input.
    /// Computing for more than a moment: say so in the bottom row, with
    /// the function running and the memory in use.
    fn busy(&mut self, con: &Console) {
        let since = *self.busy_since.get_or_insert_with(Instant::now);
        if !self.status_info || self.cols < 30 || since.elapsed() < Duration::from_millis(400) {
            return;
        }
        let mut s = String::from("… working");
        if !con.running.is_empty() {
            s += &format!("  @{}", con.running);
        }
        // where the live counter sits in the text (chars from the start, its
        // length) and its band, to colour it (the peak after it stays plain)
        let mut mem_at: Option<(usize, usize, crate::sys::MemoryLevel)> = None;
        if let Some(m) = self.memory().filter(|_| self.show_memory) {
            s += "  ";
            if let Some(l) = self.mem_level() {
                mem_at = Some((printable(&s).chars().count(), m.find(" MB").map_or(m.len(), |i| i + 3), l));
            }
            s += &m;
        }
        let mut line = String::new();
        let mut w = 0;
        for c in printable(&s).chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if w + cw >= self.cols {
                break;
            }
            line.push(c);
            w += cw;
        }
        let shown = line.chars().count();
        let text = match mem_at.filter(|&(at, len, _)| at + len <= shown) {
            Some((at, len, lvl)) => {
                let pre: String = line.chars().take(at).collect();
                let live: String = line.chars().skip(at).take(len).collect();
                let post: String = line.chars().skip(at + len).collect();
                format!("{}\x1b[0m{}{}\x1b[0;2m{}", pre, self.mem_fg(lvl), live, post)
            }
            None => line,
        };
        write_all(format!("\x1b[{};1H\x1b[0;2m{}\x1b[0m\x1b[K", self.rows, text).as_bytes());
        // the next frame must repaint the row, even if nothing else changed
        self.last_frame = 0;
    }

    fn poll_interrupt(&mut self) -> bool {
        if self.input_waiting() {
            self.read_bytes(0);
        }
        if let Some(i) = self.pending.iter().position(|&b| b == 0x03) {
            self.pending.drain(..=i);
            return true;
        }
        self.hangup
    }

    /// Restore the terminal (mouse off, main screen, cooked mode).
    fn set_game(&mut self, game: &std::path::Path, save_dir: &std::path::Path) {
        self.game = Some((game.to_path_buf(), save_dir.to_path_buf()));
    }
    fn shutdown(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut out = Vec::new();
        self.gfx.shutdown(&mut out);
        if !(self.term == "linux" || self.term.starts_with("fbterm")) {
            out.extend_from_slice(b"\x1b[>4m");
        }
        out.extend_from_slice(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[0m\x1b[?25h\x1b[?1049l");
        write_all(&out);
        if let Some(n) = &self.notice {
            eprintln!("emuera-tty: {}", n);
        }
        self.raw_mode.restore();
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{char_cells, console_lookalike, Ambi};

    #[test]
    fn recolor_pairs() {
        let r = crate::console::Rgb;
        assert_eq!(
            super::parse_recolor("01183c:111d3b, #000000:0x202020, bad, 12:zz"),
            vec![(r(1, 24, 60), r(0x11, 0x1d, 0x3b)), (r(0, 0, 0), r(0x20, 0x20, 0x20))]
        );
        assert!(super::parse_recolor("").is_empty());
    }

    #[test]
    fn osc66_draws_one_two_cell_glyph() {
        assert_eq!(char_cells('━', Ambi::Osc66, None), (2, 0, ' '));
        assert_eq!(char_cells('林', Ambi::Osc66, None), (2, 0, ' '));
        assert_eq!(char_cells('a', Ambi::Osc66, None), (1, 0, ' '));
    }

    #[test]
    fn emuera_widths_on_a_narrow_terminal() {
        assert_eq!(char_cells('a', Ambi::Pad, None), (1, 0, ' '));
        assert_eq!(char_cells('林', Ambi::Pad, None), (2, 0, ' ')); // already wide
        assert_eq!(char_cells('━', Ambi::Pad, None), (1, 1, '━')); // line continues
        assert_eq!(char_cells('┏', Ambi::Pad, None), (1, 1, '━'));
        assert_eq!(char_cells('┃', Ambi::Pad, None), (1, 1, ' ')); // no rightward stroke
        assert_eq!(char_cells('■', Ambi::Pad, None), (1, 1, '■'));
        assert_eq!(char_cells('※', Ambi::Pad, None), (1, 1, ' '));
        assert_eq!(char_cells('―', Ambi::Pad, None), (1, 1, '―')); // horizontal bar continues
        assert_eq!(char_cells('ｱ', Ambi::Pad, None), (1, 0, ' ')); // half-width kana: 1 in Emuera too
    }

    #[test]
    fn measured_width_wins() {
        // a terminal seen drawing ━ two cells wide needs no filler
        assert_eq!(char_cells('━', Ambi::Pad, Some(2)), (2, 0, '━'));
        // one that draws ￤ narrow (tables say 2) gets a filler instead
        assert_eq!(char_cells('￤', Ambi::Pad, Some(1)), (1, 1, ' '));
    }

    #[test]
    fn linux_console_draws_everything_in_one_cell() {
        // kanji, full-width space and letters: one cell plus a filler
        // the Linux console's font: missing glyphs drawn as lookalikes
        let has: std::collections::HashSet<char> = ['─', '■', '·'].into_iter().collect();
        assert_eq!(console_lookalike('【', &has), '[');
        assert_eq!(console_lookalike('）', &has), ')');
        assert_eq!(console_lookalike('Ａ', &has), 'A');
        assert_eq!(console_lookalike('ー', &has), '─');
        assert_eq!(console_lookalike('・', &has), '·');
        assert_eq!(console_lookalike('●', &has), 'o');
        assert_eq!(console_lookalike('漢', &has), '漢');
        assert_eq!(char_cells('漢', Ambi::Narrow, None), (1, 1, ' '));
        assert_eq!(char_cells('　', Ambi::Narrow, None), (1, 1, ' '));
        assert_eq!(char_cells('Ａ', Ambi::Narrow, None), (1, 1, ' '));
        assert_eq!(char_cells('━', Ambi::Narrow, None), (1, 1, '━'));
        assert_eq!(char_cells('a', Ambi::Narrow, None), (1, 0, ' '));
    }

    #[test]
    fn no_filler_when_terminal_draws_ambiguous_wide() {
        assert_eq!(char_cells('━', Ambi::Wide, None), (2, 0, '━'));
        assert_eq!(char_cells('■', Ambi::Wide, None), (2, 0, '■'));
    }
}

#[cfg(test)]
mod translate_tests {
    /// A translation without the speech's line breaks is spread over its
    /// lines by their lengths.
    #[test]
    fn spread_by_length() {
        let w = |s: &str| s.chars().count();
        assert_eq!(super::spread("one two three four", &[10, 10], w), ["one two", "three four"]);
        assert_eq!(super::spread("a b c d e f", &[2, 10], w), ["a", "b c d e f"]);
        assert_eq!(super::spread("あいうえおか", &[3, 3], w), ["あいう", "えおか"]);
        assert_eq!(super::spread("solo", &[5, 5, 5], w), ["solo", "", ""]);
    }
}
