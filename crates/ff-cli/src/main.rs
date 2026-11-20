use std::env;
use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use ff_core::{BuildOptions, CaseMode, Index, Kind, Query, SearchOptions, Sort};

mod tui;

const USAGE: &str = "\
ff - fast find: instant file search

USAGE:
  ff [OPTIONS] TERM...          search (all terms must match)
  ff -i                         search interactively in the terminal
  ff index [ROOT...] [-x DIR]   build or rebuild the index
  ff stats                      show index information

TERMS:
  foo        name contains \"foo\" (case-insensitive unless it has uppercase)
  '*.pdf'    wildcard on the whole name (* any run, ? one character)
  src/main   contains '/': matched against the full path
  '!foo'     must not match

FILTERS (mix them with terms):
  ext:pdf            by extension; several: ext:jpg;png   compound: ext:tar.gz
  size:'>100M'       by size, also >= < <= and ranges: size:10M..1G
  modified:today     by date: yesterday, 3h, 7d, 2026-10-24,
                     2026-01-01..2026-06-30   (dm: for short)

SEARCH OPTIONS:
  -f, --files        files only
  -d, --dirs         directories only
  -c, --case         case-sensitive
  -i, --ignore-case  case-insensitive (alone, with no TERM: interactive)
  -I, --interactive  search interactively, starting with the TERMs if any.
                     Enter prints the chosen path: vim \"$(ff -i)\";
                     F1 there lists the keys
  -R, --regex        terms are regular expressions, e.g. '^img_[0-9]+[.]jpe?g$'
  -w, --word         terms match whole words only
      --match-path   match every term against the full path
      --wildcards-anywhere   a wildcard may match part of the name
  -n, --limit N      at most N results
  -s, --sort KEY     name | path | size | mtime
  -r, --reverse      reverse the order
  -l, --long         show size and modification time
  -0, --null         separate results with NUL instead of newline
      --count        print only the number of matches
  -o, --open         open the first result with its default application
  -e, --edit         open the first result in the default text editor
      --open-folder  show the first result selected in the file manager
  -p, --properties   show the properties of the first result

INDEX OPTIONS:
  -x, --exclude DIR  skip a directory (repeatable)
      --no-hidden    skip files and folders whose name starts with a dot
  -j, --threads N    walker threads
      --device DEV   also index an ext2/3/4 or NTFS partition (or disk image)
                     directly, without mounting it (repeatable; a real
                     partition needs root). Alone, it indexes only that.
  With no ROOT, the previous roots and excludes are reused (first run: /).

GENERAL:
      --db FILE      index file (default: $FF_DB, else
                     /run/ff/index.db when the ffd daemon runs,
                     else $XDG_CACHE_HOME/ff/index.db)
  -h, --help         -V, --version
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("ff: {msg}");
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct Args {
    free: Vec<OsString>,
    db: Option<PathBuf>,
    kind: Kind,
    case: CaseMode,
    regex: bool,
    word: bool,
    match_path: bool,
    wildcards_anywhere: bool,
    sort: Sort,
    reverse: bool,
    limit: usize,
    long: bool,
    null: bool,
    count: bool,
    open: bool,
    open_folder: bool,
    edit: bool,
    properties: bool,
    excludes: Vec<PathBuf>,
    devices: Vec<PathBuf>,
    no_hidden: bool,
    threads: usize,
    interactive: bool,
    /// -i was given: alone it means the interactive search.
    short_i: bool,
}

fn run() -> Result<ExitCode, String> {
    let mut a = Args::default();
    let mut it = env::args_os().skip(1);
    let mut only_free = false;
    while let Some(arg) = it.next() {
        let Some(flag) = arg.to_str().filter(|s| !only_free && s.len() > 1 && s.starts_with('-')) else {
            a.free.push(arg);
            continue;
        };
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        let number = |v: OsString, name: &str| {
            v.to_str().and_then(|s| s.parse::<usize>().ok()).ok_or(format!("{name} needs a number"))
        };
        match flag {
            "--" => only_free = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            "-V" | "--version" => {
                println!("ff {}", env!("CARGO_PKG_VERSION"));
                return Ok(ExitCode::SUCCESS);
            }
            "-f" | "--files" => a.kind = Kind::File,
            "-d" | "--dirs" => a.kind = Kind::Dir,
            "-c" | "--case" => a.case = CaseMode::Sensitive,
            "-i" => (a.case, a.short_i) = (CaseMode::Insensitive, true),
            "--ignore-case" => a.case = CaseMode::Insensitive,
            "-I" | "--interactive" => a.interactive = true,
            "-R" | "--regex" => a.regex = true,
            "-w" | "--word" => a.word = true,
            "--match-path" => a.match_path = true,
            "--wildcards-anywhere" => a.wildcards_anywhere = true,
            "-r" | "--reverse" => a.reverse = true,
            "-l" | "--long" => a.long = true,
            "-0" | "--null" => a.null = true,
            "--count" => a.count = true,
            "-o" | "--open" => a.open = true,
            "--open-folder" => a.open_folder = true,
            "-e" | "--edit" => a.edit = true,
            "-p" | "--properties" => a.properties = true,
            "-n" | "--limit" => a.limit = number(value(flag)?, flag)?,
            "-j" | "--threads" => a.threads = number(value(flag)?, flag)?,
            "-x" | "--exclude" => a.excludes.push(value(flag)?.into()),
            "--device" => a.devices.push(value(flag)?.into()),
            "--no-hidden" => a.no_hidden = true,
            "--db" => a.db = Some(value(flag)?.into()),
            "-s" | "--sort" => {
                a.sort = match value(flag)?.to_str() {
                    Some("name") => Sort::Name,
                    Some("path") => Sort::Path,
                    Some("size") => Sort::Size,
                    Some("mtime") => Sort::Mtime,
                    _ => return Err("--sort takes name, path, size or mtime".into()),
                }
            }
            _ => return Err(format!("unknown option {flag} (see ff --help)")),
        }
    }

    let db = a.db.take().map_or_else(default_db, Ok)?;
    if a.short_i && a.free.is_empty() && !a.interactive {
        // `ff -i` alone: there is nothing to ignore the case of.
        (a.interactive, a.case) = (true, CaseMode::Smart);
    }
    if a.interactive {
        return cmd_interactive(a, &db);
    }
    // A leading "index"/"stats" is a command unless "--" was used.
    match a.free.first().and_then(|f| f.to_str()).filter(|_| !only_free) {
        Some("index") => cmd_index(&a, &db),
        Some("stats") => cmd_stats(&db),
        Some("help") => {
            print!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        _ if a.free.is_empty() => {
            eprint!("{USAGE}");
            Ok(ExitCode::from(2))
        }
        _ => cmd_search(&a, &db),
    }
}

fn default_db() -> Result<PathBuf, String> {
    ff_core::default_db_path().ok_or_else(|| "HOME is not set; use --db".into())
}

fn load(db: &PathBuf) -> Result<Index, String> {
    Index::load(db).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("no index at {}; run `ff index` first", db.display()),
        _ => format!("{}: {e}", db.display()),
    })
}

fn cmd_index(a: &Args, db: &PathBuf) -> Result<ExitCode, String> {
    if db.as_os_str() == ff_core::SYSTEM_DB {
        return Err(format!("{} is kept up to date by the ffd daemon; use --db FILE for a private index", db.display()));
    }
    let roots: Vec<PathBuf> = a.free[1..].iter().map(PathBuf::from).collect();
    let mut opts = if !roots.is_empty() {
        BuildOptions::new(roots)
    } else {
        // Refresh: same roots and excludes as last time (first run: /).
        BuildOptions::refresh(&Index::load(db).unwrap_or_default())
    };
    for x in &a.excludes {
        if !opts.excludes.contains(x) {
            opts.excludes.push(x.clone());
        }
    }
    opts.threads = a.threads;
    opts.skip_hidden |= a.no_hidden;

    let start = Instant::now();
    // With only devices named, index just those; otherwise walk as usual.
    let only_devices = !a.devices.is_empty() && a.free.len() == 1;
    let mut index = if only_devices { Index::new() } else { Index::build(&opts).map_err(|e| e.to_string())? };
    for device in &a.devices {
        let name = ff_core::os::path_bytes(device);
        let read = ff_core::rawfs::scan(device, name).map_err(|e| format!("{}: {e}", device.display()))?;
        index.append(&read);
    }
    index.save(db).map_err(|e| format!("{}: {e}", db.display()))?;
    eprintln!(
        "indexed {} entries in {:.2}s -> {}",
        index.len(),
        start.elapsed().as_secs_f64(),
        db.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn cmd_stats(db: &PathBuf) -> Result<ExitCode, String> {
    let index = load(db)?;
    // A closed pipe (`ff stats | head -3`) just ends the output.
    let _ = print_stats(db, &index);
    Ok(ExitCode::SUCCESS)
}

fn print_stats(db: &PathBuf, index: &Index) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let dirs = (0..index.len() as u32).filter(|&i| index.is_dir(i)).count();
    let file_len = std::fs::metadata(db).map_or(0, |m| m.len());
    writeln!(out, "index:    {} ({:.1} MiB)", db.display(), file_len as f64 / 1048576.0)?;
    writeln!(out, "entries:  {} ({} directories, {} files)", index.len(), dirs, index.len() - dirs)?;
    writeln!(out, "memory:   {:.1} MiB", index.memory_usage() as f64 / 1048576.0)?;
    for r in index.roots() {
        writeln!(out, "root:     {}", index.path(r).display())?;
    }
    for x in index.excludes() {
        writeln!(out, "exclude:  {}", x.display())?;
    }
    if index.skips_hidden() {
        writeln!(out, "exclude:  hidden files and folders (names starting with a dot)")?;
    }
    Ok(())
}

fn cmd_interactive(a: Args, db: &PathBuf) -> Result<ExitCode, String> {
    let index = load(db)?;
    let query = a.free.iter().map(|t| t.to_string_lossy()).collect::<Vec<_>>().join(" ");
    let start = tui::Start {
        query,
        kind: a.kind,
        case: a.case,
        regex: a.regex,
        word: a.word,
        match_path: a.match_path,
        wildcards_anywhere: a.wildcards_anywhere,
        sort: a.sort,
        reverse: a.reverse,
        null: a.null,
    };
    tui::run(db.clone(), index, start)
}

fn cmd_search(a: &Args, db: &PathBuf) -> Result<ExitCode, String> {
    let index = load(db)?;
    let options = ff_core::QueryOptions {
        case: a.case,
        regex: a.regex,
        whole_word: a.word,
        match_path: a.match_path,
        wildcards_anywhere: a.wildcards_anywhere,
    };
    let query = Query::from_terms_opts(a.free.iter().map(|t| ff_core::os::bytes(t)), &options);
    if let Some(problem) = query.error() {
        return Err(problem.to_string());
    }
    let opts = SearchOptions {
        kind: a.kind,
        sort: a.sort,
        reverse: a.reverse,
        limit: a.limit,
        threads: 0,
    };
    let hits = index.search(&query, &opts);
    let found = if hits.is_empty() { ExitCode::from(1) } else { ExitCode::SUCCESS };

    if (a.open || a.open_folder || a.edit || a.properties) && !hits.is_empty() {
        let target = index.path(hits[0]);
        if a.properties {
            let props = ff_core::desktop::properties(&target).map_err(|e| format!("{}: {e}", target.display()))?;
            for (label, value) in props {
                println!("{:<12} {value}", format!("{label}:"));
            }
            return Ok(ExitCode::SUCCESS);
        }
        println!("{}", target.display());
        let done = if a.open_folder {
            ff_core::desktop::reveal(&target)
        } else if a.edit {
            ff_core::desktop::open_in_editor(&target)
        } else {
            ff_core::desktop::open(&target)
        };
        return done.map(|()| ExitCode::SUCCESS).map_err(|e| e.to_string());
    }

    // A closed pipe (e.g. `ff foo | head`) just ends the output.
    let _ = print_hits(a, &index, &hits);
    Ok(found)
}

fn print_hits(a: &Args, index: &Index, hits: &[u32]) -> io::Result<()> {
    let mut out = BufWriter::with_capacity(1 << 16, io::stdout().lock());
    if a.count {
        writeln!(out, "{}", hits.len())?;
        return out.flush();
    }
    let mut path = Vec::new();
    for &id in hits {
        if a.long {
            let (y, mo, d, h, mi) = ff_core::local_civil(index.mtime(id));
            let kind = if index.is_dir(id) { 'd' } else if index.is_symlink(id) { 'l' } else { '-' };
            write!(out, "{kind} {:>12} {y:04}-{mo:02}-{d:02} {h:02}:{mi:02}  ", index.size(id))?;
        }
        index.write_path(id, &mut path);
        out.write_all(&path)?;
        out.write_all(if a.null { b"\0" } else { b"\n" })?;
    }
    out.flush()
}
