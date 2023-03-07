use clap::Parser;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Stylize,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs::File,
    io::stdout,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

#[derive(Parser)]
#[command(author, version, about)]
struct Opts {
    /// ask for confirmation before exiting the program
    #[arg(short = 'c', long = "confirm")]
    confirm: bool,

    /// clear screen before running task
    #[arg(long = "clear")]
    clear: bool,
}

const TTR_CONFIG: &str = ".ttr.yaml";

type Result<T> = anyhow::Result<T>;

#[derive(Deserialize, Debug)]
struct Task {
    name: String,
    key: char,
    cmd: String,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    clear: bool,
}

struct AlternateScreen;

impl AlternateScreen {
    fn enter() -> Result<Self> {
        execute!(stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        // No need to unpack Result. We can't do anythere here anyway
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    let mut tasks = deduplicate_tasks(read_tasks()?);
    tasks.sort_by(|a, b| a.name.cmp(&b.name));
    let Some(task) = select_task(&tasks)? else {
        return Ok(())
    };

    'task_loop: loop {
        if task.clear || opts.clear {
            execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
        }
        let exit_status = create_process(task).wait().expect("Process failed");

        if exit_status.success() && !task.confirm && !opts.confirm {
            break 'task_loop;
        }

        println!();
        let prefix = "   ";
        if exit_status.success() {
            println!("{}Task {}.", prefix, "completed".stylize().green().bold(),);
        } else {
            println!(
                "{}Task {} ({}).",
                prefix,
                "failed".stylize().red().bold(),
                exit_status,
            );
        };
        println!(
            "{}Press {} to continue or {}epeat...",
            prefix,
            "Enter".stylize().yellow().bold(),
            "r".stylize().yellow().bold()
        );

        'confirmation_loop: loop {
            match read_key_code()? {
                KeyCode::Enter | KeyCode::Char('q') => break 'task_loop,
                KeyCode::Char('r') => continue 'task_loop,
                _ => continue 'confirmation_loop,
            };
        }
    }

    Ok(())
}

/// Dediplicates task by checking if there tasks assigned to the same key.
///
/// If there are, the earlier task will win and latter one will be remove from the result
fn deduplicate_tasks(tasks: Vec<Task>) -> Vec<Task> {
    let mut duplicates = HashSet::new();
    tasks
        .into_iter()
        .filter(|t| duplicates.insert(t.key))
        .collect::<Vec<_>>()
}

fn read_tasks() -> Result<Vec<Task>> {
    let mut tasks = vec![];
    if let Some(config) = cwd_config() {
        tasks.extend(read_tasks_from_file(config)?);
    }
    if let Some(config) = user_config() {
        tasks.extend(read_tasks_from_file(config)?);
    }
    Ok(tasks)
}

fn read_tasks_from_file(path: impl AsRef<Path>) -> Result<Vec<Task>> {
    let file = File::open(path)?;
    Ok(serde_yaml::from_reader(file)?)
}

fn user_config() -> Option<PathBuf> {
    dirs::home_dir()
        .map(|home| home.join(TTR_CONFIG))
        .filter(|config| config.is_file())
}

fn cwd_config() -> Option<PathBuf> {
    let path = PathBuf::from(TTR_CONFIG);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

fn create_process(task: &Task) -> Child {
    let arg = format!("exec {}", task.cmd);
    Command::new("sh")
        .args(["-c", arg.as_str()])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Unable to start")
}

fn read_key_code() -> Result<KeyCode> {
    let KeyEvent { code, .. } = read_key_event()?;
    Ok(code)
}

fn read_key_event() -> Result<KeyEvent> {
    enable_raw_mode()?;
    let key_code = loop {
        if let Ok(true) = event::poll(Duration::from_secs(60)) {
            let event = event::read()?;
            if let Event::Key(e) = event {
                break e;
            }
        }
    };
    disable_raw_mode()?;
    Ok(key_code)
}

fn select_task(tasks: &[Task]) -> Result<Option<&Task>> {
    let _alt = AlternateScreen::enter();
    let mut stdout = stdout().lock();

    let mut error: Option<String> = None;
    loop {
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        println!();
        if !tasks.is_empty() {
            println!("    {}", "SELECT A TASK".stylize().grey());
            println!();
            let (width, _) = crossterm::terminal::size()?;

            // 4 characters is a padding from screen edge
            // 20 is width of one task representation
            let columns_fit = (width as usize - 4) / 20;
            let rows = (tasks.len() + columns_fit - 1) / columns_fit;

            let columns = tasks.chunks(rows).collect::<Vec<_>>();
            for i in 0..rows {
                print!("  ");
                for column in &columns {
                    let Some(task) = column.get(i) else {
                        break;
                    };
                    let name = if task.name.len() > 12 {
                        format!("{}…", task.name.chars().take(11).collect::<String>())
                    } else {
                        task.name.clone()
                    };
                    print!("  {} → {:12}  ", task.key.stylize().green().bold(), name);
                }
                println!();
            }
        } else {
            println!("    {}", "No tasks configured".stylize().bold());
            println!("    Create file {} in current directory", TTR_CONFIG);
        }
        println!();
        println!("    {} → {:10}", "q".stylize().red(), "quit");

        if let Some(e) = error.take() {
            println!("\n   {}\n", e.stylize().red());
        }

        let KeyEvent {
            code, modifiers, ..
        } = read_key_event()?;
        let task = match code {
            KeyCode::Char('q') => Ok(None),
            KeyCode::Char('c') if modifiers == KeyModifiers::CONTROL => Ok(None),
            KeyCode::Char(' ') => Err("Whitespace is not allowed".to_string()),
            KeyCode::Char(ch) => tasks
                .iter()
                .find(|t| t.key == ch)
                .map(Some)
                .ok_or(format!("No task for key: {}", ch)),
            _ => Err("Please enter character key".to_string()),
        };
        match task {
            Ok(task) => return Ok(task),
            Err(reason) => error = Some(reason),
        };
    }
}
