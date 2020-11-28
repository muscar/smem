use humansize::file_size_opts::{FileSizeOpts, CONVENTIONAL};
use libc::{self, pid_t, uid_t};
use os_str_bytes::OsStringBytes;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::path::PathBuf;

use self::error::Error;
use self::fields::{Field, FieldKind};
use self::filter::Filters;
use self::options::Options;
use self::stats::{ProcessInfo, ProcessSizes, Size, User};

mod error;
mod fields;
mod filter;
mod options;
mod stats;

enum UserEntry {
    User(OsString),
    FilteredOut,
}

fn build_user_map(filters: &Filters) -> HashMap<uid_t, UserEntry> {
    unsafe { users::all_users() }
        .map(|u| {
            let entry = if filters.accept_user(&u.name()) {
                UserEntry::User(u.name().to_os_string())
            } else {
                UserEntry::FilteredOut
            };
            (u.uid(), entry)
        })
        .collect()
}

fn parse_size(s: &str) -> usize {
    let s = &s[..s.len() - 4];
    let pos = s.rfind(' ').unwrap();
    let s = &s[pos + 1..];
    s.parse().unwrap_or_default()
}

fn parse_uid(s: &str) -> uid_t {
    s[4..]
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap_or_default()
}

fn get_uid(path: &PathBuf) -> Result<uid_t, Error> {
    let mut line = String::new();
    let mut reader = BufReader::new(File::open(&path.join("status"))?);
    while reader.read_line(&mut line).unwrap_or_default() > 0 {
        if line.starts_with("Uid:") {
            return Ok(parse_uid(&line));
        }
        line.clear();
    }
    Ok(0)
}

fn get_pid(path: &PathBuf) -> Option<pid_t> {
    if let Some(pid) = path
        .file_name()
        .and_then(|dir_name| dir_name.to_str())
        .and_then(|pid| pid.parse().ok())
    {
        Some(pid)
    } else {
        None
    }
}

fn get_command(path: &PathBuf) -> Result<OsString, Error> {
    let mut command = fs::read(&path.join("comm"))?;
    command.pop();
    Ok(OsString::from_bytes(&command)?)
}

fn get_cmdline(path: &PathBuf) -> Result<OsString, Error> {
    let mut cmdline = fs::read(&path.join("cmdline"))?;
    for c in &mut cmdline {
        if *c == b'\0' {
            *c = b' ';
        }
    }
    if !cmdline.is_empty() {
        cmdline.pop();
    }
    Ok(OsString::from_bytes(&cmdline)?)
}

fn open_smaps(path: &Path) -> io::Result<BufReader<File>> {
    let file = match File::open(&path.join("smaps_rollup")) {
        Ok(file) => file,
        Err(_) => File::open(&path.join("smaps"))?,
    };
    Ok(BufReader::new(file))
}

fn get_memory_info(path: &PathBuf) -> Result<ProcessSizes, Error> {
    let mut reader = open_smaps(&path)?;
    let mut line = String::new();
    let mut pss = 0;
    let mut rss = 0;
    let mut private_clean = 0;
    let mut private_dirty = 0;
    let mut swap = 0;

    while reader.read_line(&mut line).unwrap_or_default() > 0 {
        if line.starts_with("Pss:") {
            pss += parse_size(&line);
        } else if line.starts_with("Rss:") {
            rss += parse_size(&line);
        } else if line.starts_with("Private_Clean:") {
            private_clean += parse_size(&line);
        } else if line.starts_with("Private_Dirty:") {
            private_dirty += parse_size(&line);
        } else if line.starts_with("Swap:") {
            swap += parse_size(&line);
        }

        line.clear();
    }

    let uss = private_clean + private_dirty;
    return Ok(ProcessSizes {
        pss: Size(pss * 1024),
        rss: Size(rss * 1024),
        uss: Size(uss * 1024),
        swap: Size(swap * 1024),
    });
}

fn get_process_info(
    path: &PathBuf,
    filters: &Filters,
    users: &HashMap<uid_t, UserEntry>,
) -> Result<Option<ProcessInfo>, Error> {
    let pid = match get_pid(path) {
        Some(pid) => pid,
        None => return Ok(None),
    };
    let uid = get_uid(path)?;
    let username = match users.get(&uid) {
        Some(UserEntry::User(name)) => name.clone(),
        Some(UserEntry::FilteredOut) => return Ok(None),
        None => OsString::new(),
    };
    let command = get_command(path)?;
    let cmdline = get_cmdline(path)?;
    if cmdline.is_empty() || !filters.accept_process(&command) && !filters.accept_process(&cmdline)
    {
        return Ok(None);
    }

    let sizes = get_memory_info(path)?;
    let statistics = ProcessInfo {
        pid,
        user: User::new(uid, username),
        command,
        cmdline,
        sizes,
    };
    Ok(Some(statistics))
}

fn print_processes(options: &Options) -> Result<(), Error> {
    let default_fields = vec![
        Field::Pid,
        Field::User,
        Field::Pss,
        Field::Rss,
        Field::Uss,
        Field::Swap,
        Field::Cmdline,
    ];

    let active_fields = if options.fields.is_empty() {
        &default_fields
    } else {
        &options.fields
    };

    let mut filters = filter::Filters::new();
    if let Some(ref process) = options.process_filter {
        filters.process(process);
    }
    if let Some(ref user) = options.user_filter {
        filters.user(user);
    }

    let users = build_user_map(&filters);
    let entries = fs::read_dir(&options.source)
        .unwrap_or_else(|e| panic!("can't read {}: {}", options.source.display(), e))
        .filter_map(|e| e.ok())
        .collect::<Vec<_>>();
    let mut processes = entries
        .par_iter()
        .filter_map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => get_process_info(&e.path(), &filters, &users).ok(),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();

    if !options.no_header {
        for c in active_fields {
            if c.kind(&options) == FieldKind::Text {
                print!("{:<10} ", c.name());
            } else {
                print!("{:>10} ", c.name());
            }
        }
        println!();
    }
    let sort_field = options.sort_field.unwrap_or(Field::Rss);
    if options.reverse {
        processes.sort_by(|p1, p2| p1.cmp_by(sort_field, p2, &options).reverse());
    } else {
        processes.sort_by(|p1, p2| p1.cmp_by(sort_field, p2, &options));
    }
    let file_size_opts = FileSizeOpts {
        space: false,
        ..CONVENTIONAL
    };
    let mut totals = ProcessSizes::new();
    for process in processes {
        for &c in active_fields {
            process
                .format_field(io::stdout(), c, &options, &file_size_opts)
                .unwrap();
            print!(" ");
        }
        println!();
        totals += process.sizes;
    }
    if options.totals {
        println!(
            "--------------------------------------------------------------------------------"
        );
        for &c in active_fields {
            if c.kind(&options) == FieldKind::Size {
                totals
                    .format_field(io::stdout(), c, &options, &file_size_opts)
                    .unwrap();
                print!(" ");
            } else {
                print!("{:10} ", " ");
            }
        }
        println!();
    }
    Ok(())
}

fn run(options: &Options) -> Result<(), Error> {
    print_processes(&options)
}

fn disable_sigpipe_handling() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() {
    disable_sigpipe_handling();

    let options = Options::from_args();
    match run(&options) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{}", e);
        }
    }
}
