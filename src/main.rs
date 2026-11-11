//! GitKrill - a fast, native Git client with a commit graph.

// No console window behind the GUI in release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod avatars;
mod external;
mod git;
mod git_setup;
mod hosting;
mod notification;
mod platform;
mod prefs;
mod shot;
mod state;
#[cfg(test)]
mod tests;
mod theme;
mod ui;
mod update;
mod worker;
mod workspace;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result {
    // Started by git or ssh to ask the user something (GIT_ASKPASS,
    // SSH_ASKPASS, gpg.program, gpg.x509.program): answer and leave before
    // any window opens.
    if let Some(code) = git::prompt::helper::dispatch() {
        std::process::exit(code);
    }

    let mut args = std::env::args().skip(1);
    let first = args.next();

    match first.as_deref() {
        Some("--version" | "-V") => {
            println!("gitkrill {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!(
                "Usage: gitkrill [PATH]\n\nOpens the Git repository containing PATH.\n\n  \
                 --git       print which git GitKrill uses, and its version\n  \
                 --version   print GitKrill's version"
            );
            return Ok(());
        }
        // Which git the app would run, for checking an installation.
        Some("--git") => {
            let found = git::cli::locate();
            match &found {
                Some(path) => println!("{}", path.display()),
                None => println!("{}", git::cli::NOT_FOUND),
            }
            match found.map(|_| git::cli::version()) {
                Some(Ok(version)) => println!("{version}"),
                Some(Err(why)) => {
                    println!("{why}");
                    std::process::exit(1);
                }
                None => std::process::exit(1),
            }
            return Ok(());
        }
        // Print what the backend sees, without starting the UI.
        Some("--dump") => {
            let path = args.next().map(PathBuf::from).unwrap_or_else(|| ".".into());
            let limit = args.next().and_then(|n| n.parse().ok()).unwrap_or(state::INITIAL_LIMIT);
            dump(path, limit);
            return Ok(());
        }
        _ => {}
    }

    let open = first.map(PathBuf::from).map(|p| std::path::absolute(&p).unwrap_or(p));
    let size = std::env::var("GITKRILL_SHOT_SIZE")
        .ok()
        .and_then(|v| {
            let (w, h) = v.split_once('x')?;
            Some([w.parse().ok()?, h.parse().ok()?])
        })
        .unwrap_or([1440.0, 880.0]);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("GitKrill")
        .with_inner_size(size)
        .with_min_inner_size([940.0, 540.0])
        .with_icon(theme::app_icon())
        .with_drag_and_drop(true);
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        // Scripted screenshot runs should not get in the way of real work:
        // no focus, no taskbar entry, and parked off-screen unless asked.
        viewport = viewport.with_active(false).with_taskbar(false);
        if std::env::var_os("GITKRILL_SHOT_VISIBLE").is_none() {
            viewport = viewport.with_position([-20000.0, -20000.0]);
        }
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "GitKrill",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, open)))),
    )
}

fn dump(path: PathBuf, limit: usize) {
    use git::graph::SegKind;

    let started = std::time::Instant::now();
    let mut loader = match git::repo::Loader::open(&path) {
        Ok(loader) => loader,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let snap = match loader.snapshot(limit, &Default::default()) {
        Ok(snap) => snap,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let elapsed = started.elapsed();

    println!("workdir   {}", snap.workdir.display());
    println!("head      {} {:?}", snap.head_label(), snap.head.oid.map(git::short));
    println!("state     {:?}", snap.state);
    println!(
        "refs      {} local, {} remote, {} tags, {} stashes",
        snap.branches.len(),
        snap.remote_branches.len(),
        snap.tags.len(),
        snap.stashes.len()
    );
    for b in &snap.branches {
        println!("  {}{} -> {:?} +{} -{}", if b.is_head { "* " } else { "  " }, b.name, b.upstream, b.ahead, b.behind);
    }
    println!("status    {} unstaged, {} staged", snap.status.unstaged.len(), snap.status.staged.len());
    for f in &snap.status.unstaged {
        println!("  U {:?} {}", f.kind, f.path);
    }
    for f in &snap.status.staged {
        println!("  S {:?} {}", f.kind, f.path);
    }
    println!(
        "history   {} rows, {} lanes, loaded in {:.1?}",
        snap.history.commits.len(),
        snap.history.graph.max_lane + 1,
        elapsed
    );

    let width = snap.history.graph.max_lane as usize + 1;
    for (i, commit) in snap.history.commits.iter().take(40).enumerate() {
        let row = snap.history.graph.row(i);
        let (lane, segs) = (row.lane, row.segs);
        let mut cells = vec![' '; width];
        for seg in segs {
            let cell = &mut cells[seg.lane as usize];
            *cell = match (seg.kind, *cell) {
                (SegKind::Through, _) => '|',
                (SegKind::In, ' ') => '\\',
                (SegKind::Out, ' ') => '/',
                (_, other) => other,
            };
        }
        cells[lane as usize] = match commit.kind {
            git::RowKind::Wip => 'o',
            git::RowKind::Stash => '#',
            git::RowKind::Commit => '*',
        };
        let labels = snap
            .refs_by_oid
            .get(&commit.oid)
            .map(|l| l.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        println!(
            "  {} {} {} {}",
            cells.iter().collect::<String>(),
            git::short(commit.oid),
            commit.summary,
            if labels.is_empty() { String::new() } else { format!("[{labels}]") }
        );
    }
}
