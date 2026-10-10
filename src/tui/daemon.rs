//! Background daemon: draws all party Pokemon bouncing freely around the
//! terminal. Cursor is saved/restored on every frame so typing is unaffected.

use std::{
    fs::OpenOptions,
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::Result;
use rand::{rngs::SmallRng, SeedableRng};

use crate::{
    animation::walk::Walker,
    renderer::{
        iterm2::{self, Iterm2Frames},
        kitty::{image_id, KittyRenderer},
        unicode::{self, RenderedFrame},
        RendererKind,
    },
    sprite::{self, gif::DecodedFrame, sheet::SpriteSheet},
    state::{party::Party, persist, pet::PetStatus},
};

const FRAME_MS: u64 = 125; // 8 fps

static RUNNING: AtomicBool = AtomicBool::new(true);

// ── Platform: TTY path ────────────────────────────────────────────────────────

#[cfg(unix)]
const TTY_PATH: &str = "/dev/tty";

/// On Windows, CONOUT$ is the direct console output handle — same role as /dev/tty.
#[cfg(windows)]
const TTY_PATH: &str = "CONOUT$";

// ── Platform: terminal size ───────────────────────────────────────────────────

/// Query the current terminal size. Uses crossterm which handles all platforms.
fn tty_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

// ── Per-pet state ─────────────────────────────────────────────────────────────

struct PetActor {
    species_id: u16,
    shiny: bool,
    walker: Walker,
    frame_idx: usize,
    uframes: Option<UnicodeFrames>,
    iframes: Option<Iterm2Frames>,
    prev_col: Option<u16>,
    prev_row: Option<u16>,
}

impl PetActor {
    async fn new(
        species_id: u16,
        shiny: bool,
        cols: u16,
        rows: u16,
        sprite_cols: u16,
        sprite_rows: u16,
        rng: &mut SmallRng,
        renderer_kind: RendererKind,
        kitty: &mut KittyRenderer,
        out: &mut impl Write,
    ) -> Self {
        let sheet = sprite::load_sheet(species_id, shiny).await.ok();

        let uframes = if renderer_kind == RendererKind::Unicode {
            sheet.as_ref().map(UnicodeFrames::prerender)
        } else {
            None
        };

        let iframes = if renderer_kind == RendererKind::Iterm2 {
            sheet.as_ref().map(Iterm2Frames::prerender)
        } else {
            None
        };

        if renderer_kind == RendererKind::Kitty {
            if let Some(s) = &sheet {
                for &is_walk in &[false, true] {
                    let frames = if is_walk { s.walk_frames() } else { s.idle_frames() };
                    for &flip in &[false, true] {
                        let _ = kitty.upload(
                            image_id(s.species_id, s.shiny, is_walk, flip),
                            frames, flip, out,
                        );
                    }
                }
            }
        }

        // Stagger starting positions so they don't all stack up
        let walker = Walker::new(cols, rows, sprite_cols, sprite_rows, rng);

        Self {
            species_id,
            shiny,
            walker,
            frame_idx: 0,
            uframes,
            iframes,
            prev_col: None,
            prev_row: None,
        }
    }

    fn tick(&mut self, rng: &mut SmallRng) {
        self.walker.tick(rng);
        self.frame_idx = self.frame_idx.wrapping_add(1);
    }

    fn erase(&mut self, out: &mut impl Write, renderer_kind: RendererKind) {
        if let (Some(pc), Some(pr)) = (self.prev_col, self.prev_row) {
            match renderer_kind {
                RendererKind::Unicode => { let _ = unicode::clear_frame(out, pc, pr); }
                RendererKind::Iterm2  => { let _ = iterm2::clear_frame(out, pc, pr); }
                RendererKind::Kitty   => {} // Kitty uses delete-all before each redraw
            }
        }
    }

    fn draw(
        &mut self,
        out: &mut impl Write,
        renderer_kind: RendererKind,
        kitty: &mut KittyRenderer,
    ) {
        let col     = self.walker.col();
        let row     = self.walker.row();
        let is_walk = self.walker.is_walking();
        let flip    = self.walker.facing_left();

        match renderer_kind {
            RendererKind::Unicode => {
                if let Some(uf) = &self.uframes {
                    let _ = unicode::draw_frame(
                        out, uf.get(is_walk, flip, self.frame_idx), col, row,
                    );
                }
            }
            RendererKind::Iterm2 => {
                if let Some(iframes) = &self.iframes {
                    let encoded = iframes.get(is_walk, flip, self.frame_idx);
                    let _ = iterm2::draw_frame(out, encoded, col, row);
                }
            }
            RendererKind::Kitty => {
                let id = image_id(self.species_id, self.shiny, is_walk, flip);
                let _ = kitty.place(id, col, row, out);
            }
        }

        self.prev_col = Some(col);
        self.prev_row = Some(row);
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(renderer_kind: RendererKind) -> Result<()> {
    // Cross-platform signal/Ctrl-C handler
    ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::Relaxed);
    })?;

    let mut out = OpenOptions::new()
        .write(true)
        .read(true)
        .open(TTY_PATH)
        .map_err(|e| anyhow::anyhow!("cannot open terminal ({TTY_PATH}): {e}"))?;

    let (mut cols, mut rows) = tty_size();

    let party = persist::load()?;

    // Collect all alive pets from party slots
    let alive: Vec<(u16, bool)> = party
        .slots
        .iter()
        .flatten()
        .filter(|p| matches!(p.status, PetStatus::Alive))
        .map(|p| (p.species_id, p.shiny))
        .collect();

    if alive.is_empty() {
        return Ok(());
    }

    let (sprite_cols, sprite_rows) = match renderer_kind {
        RendererKind::Kitty   => (6u16, 3u16),
        RendererKind::Iterm2  => (iterm2::SPRITE_COLS, iterm2::SPRITE_ROWS),
        RendererKind::Unicode => (unicode::SPRITE_COLS, unicode::SPRITE_ROWS),
    };

    let mut rng = SmallRng::from_entropy();
    let mut kitty = KittyRenderer::new();

    // Build one actor per alive pet
    let mut actors: Vec<PetActor> = Vec::with_capacity(alive.len());
    for (species_id, shiny) in alive {
        let actor = PetActor::new(
            species_id, shiny,
            cols, rows,
            sprite_cols, sprite_rows,
            &mut rng,
            renderer_kind,
            &mut kitty,
            &mut out,
        ).await;
        actors.push(actor);
    }

    let mut last_tick = Instant::now();

    save_cursor(&mut out)?;
    draw_hint(&mut out, cols, rows, &party)?;
    restore_cursor(&mut out)?;
    out.flush()?;

    // ── Main loop ─────────────────────────────────────────────────────────────
    while RUNNING.load(Ordering::Relaxed) {
        let (new_cols, new_rows) = tty_size();
        if new_cols != cols || new_rows != rows {
            cols = new_cols;
            rows = new_rows;
            for a in &mut actors {
                a.walker.resize(cols, rows);
                a.prev_col = None;
                a.prev_row = None;
            }
            if renderer_kind == RendererKind::Kitty {
                kitty.on_resize();
            }
        }

        let now = Instant::now();
        if now.duration_since(last_tick) < Duration::from_millis(FRAME_MS) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        last_tick = now;

        // Advance all walkers
        for a in &mut actors {
            a.tick(&mut rng);
        }

        save_cursor(&mut out)?;

        // Erase all previous positions first (avoids partial overdraw).
        // Kitty: single delete-all per frame, then place all pets.
        if renderer_kind == RendererKind::Kitty {
            kitty.delete_all(&mut out)?;
        } else {
            for a in &mut actors {
                a.erase(&mut out, renderer_kind);
            }
        }
        // Draw all at new positions
        for a in &mut actors {
            a.draw(&mut out, renderer_kind, &mut kitty);
        }

        restore_cursor(&mut out)?;
        out.flush()?;
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    save_cursor(&mut out)?;
    for a in &mut actors {
        a.erase(&mut out, renderer_kind);
    }
    if renderer_kind == RendererKind::Kitty {
        kitty.delete_all(&mut out)?;
    }
    write!(out, "\x1b[{};1H\x1b[2K", rows)?; // clear hint row
    restore_cursor(&mut out)?;
    out.flush()?;

    Ok(())
}

// ── Terminal helpers ──────────────────────────────────────────────────────────

#[inline]
fn save_cursor(out: &mut impl Write) -> Result<()> {
    write!(out, "\x1b7")?;
    Ok(())
}

#[inline]
fn restore_cursor(out: &mut impl Write) -> Result<()> {
    write!(out, "\x1b8")?;
    Ok(())
}

fn draw_hint(out: &mut impl Write, cols: u16, rows: u16, party: &Party) -> Result<()> {
    let names: Vec<String> = party
        .slots
        .iter()
        .flatten()
        .filter(|p| matches!(p.status, PetStatus::Alive))
        .map(|p| p.display_name())
        .collect();

    let label = if names.is_empty() {
        "No Pokemon".to_string()
    } else {
        names.join(", ")
    };

    let hint = format!("  {}  ·  petcli stop", label);
    let w = cols as usize;
    let padded = format!("{:<width$}", hint, width = w);

    write!(
        out,
        "\x1b[{};1H\x1b[2K\x1b[90m{}\x1b[0m",
        rows,
        &padded[..padded.len().min(w)]
    )?;
    Ok(())
}

// ── Unicode frame cache ───────────────────────────────────────────────────────

struct UnicodeFrames {
    idle:      Vec<RenderedFrame>,
    idle_flip: Vec<RenderedFrame>,
    walk:      Vec<RenderedFrame>,
    walk_flip: Vec<RenderedFrame>,
}

impl UnicodeFrames {
    fn prerender(sheet: &SpriteSheet) -> Self {
        let r = |frames: &[DecodedFrame], flip: bool| -> Vec<RenderedFrame> {
            frames.iter().map(|f| unicode::render_frame(&f.pixels, flip)).collect()
        };
        Self {
            idle:      r(sheet.idle_frames(), false),
            idle_flip: r(sheet.idle_frames(), true),
            walk:      r(sheet.walk_frames(), false),
            walk_flip: r(sheet.walk_frames(), true),
        }
    }

    fn get(&self, is_walk: bool, flip: bool, idx: usize) -> &RenderedFrame {
        let bank = match (is_walk, flip) {
            (false, false) => &self.idle,
            (false, true)  => &self.idle_flip,
            (true,  false) => &self.walk,
            (true,  true)  => &self.walk_flip,
        };
        let bank = if bank.is_empty() { &self.idle } else { bank };
        if bank.is_empty() {
            static EMPTY: Vec<Vec<(
                crossterm::style::Color,
                crossterm::style::Color,
                char,
            )>> = Vec::new();
            return &EMPTY;
        }
        &bank[idx % bank.len()]
    }
}
