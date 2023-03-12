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
    env::current_dir,
    fs::File,
    io::stdout,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
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

    /// in loop mode after task completed you can select another task to run
    #[arg(long = "loop")]
    loop_mode: bool,
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
    working_dir: Option<PathBuf>,
}

enum NextAction {
    Continue,
    Exit,
    SelectTask,
    RepeatTask,
}

struct AlternateScreen;

impl AlternateScreen {
    fn enter() -> Self {
        execute!(stdout(), EnterAlternateScreen).expect("Unable to enter alternative screen");
        Self
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        // No need to unpack Result. We can't do anything about it anyway
        let _ = execute!(stdout(), LeaveAlternateScreen);
    }
}

struct RawMode;

impl RawMode {
    fn enter() -> Self {
        enable_raw_mode().expect("Unable to enable raw mode");
        Self
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // No need to unpack Result. We can't do anything about it anyway
        let _ = disable_raw_mode();
    }
}

fn main() -> Result<()> {
    let opts = Opts::parse();
    let mut tasks = deduplicate_tasks(read_tasks()?);
    tasks.sort_by(|a, b| a.name.cmp(&b.name));

    let mut status_line: Option<String> = None;
    'select_loop: loop {
        let Some(task) = select_task(&tasks, &status_line)? else {
            return Ok(())
        };

        'task_loop: loop {
            if task.clear || opts.clear {
                execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0))?;
            }
            let exit_status = create_process(task)?.wait()?;
            status_line = Some(format_status_line(task, exit_status));

            if !exit_status.success() || task.confirm || opts.confirm {
                match confirm_task(exit_status) {
                    NextAction::Continue if opts.loop_mode => continue 'select_loop,
                    NextAction::Continue | NextAction::Exit => break 'select_loop,
                    NextAction::RepeatTask => continue 'task_loop,
                    NextAction::SelectTask => continue 'select_loop,
                }
            }
            if opts.loop_mode {
                continue 'select_loop;
            } else {
                break 'select_loop;
            }
        }
    }

    Ok(())
}

fn format_status_line(task: &Task, exit_status: ExitStatus) -> String {
    if exit_status.success() {
        let completed = "completed".stylize().green();
        format!("Task {} {}", task.name, completed)
    } else {
        let failed = "failed".stylize().red();
        format!("Task {} {} ({})", task.name, failed, exit_status)
    }
}

fn confirm_task(exit_status: ExitStatus) -> NextAction {
    // Print confirmation dialog
    println!();
    let prefix = "   ";
    if exit_status.success() {
        println!("{}Task {}", prefix, "completed".stylize().green().bold(),);
    } else {
        println!(
            "{}Task {} ({})",
            prefix,
            "failed".stylize().red().bold(),
            exit_status,
        );
    };
    println!();
    println!(
        "{}Press {} to continue. {}epeat or {}elect another task...",
        prefix,
        "Enter".stylize().yellow().bold(),
        "r".stylize().yellow().bold(),
        "s".stylize().yellow().bold(),
    );

    // Reading user decision
    loop {
        match next_key_event().code {
            KeyCode::Enter => break NextAction::Continue,
            KeyCode::Char('q') | KeyCode::Esc => break NextAction::Exit,
            KeyCode::Char('r') => break NextAction::RepeatTask,
            KeyCode::Char('s') => break NextAction::SelectTask,
            _ => continue,
        }
    }
}

/// Deduplicate tasks by checking if there are tasks assigned to the same key.
///
/// The earlier task will win and the latter will be removed from the result
fn deduplicate_tasks(tasks: Vec<Task>) -> Vec<Task> {
    let mut duplicates = HashSet::new();
    tasks
        .into_iter()
        .filter(|t| duplicates.insert(t.key))
        .collect()
}

fn read_tasks() -> Result<Vec<Task>> {
    fn tasks_from_file(path: impl AsRef<Path>) -> Result<Vec<Task>> {
        let file = File::open(path)?;
        Ok(serde_yaml::from_reader(file)?)
    }

    let mut tasks = vec![];

    // ./.ttr.yaml
    let cwd_config = Some(PathBuf::from(TTR_CONFIG)).filter(|config| config.is_file());
    if let Some(config) = cwd_config {
        tasks.extend(tasks_from_file(config)?);
    }

    // ~/.ttr.yaml
    let home_dir_config = dirs::home_dir()
        .map(|home| home.join(TTR_CONFIG))
        .filter(|config| config.is_file());
    if let Some(config) = home_dir_config {
        tasks.extend(tasks_from_file(config)?);
    }

    // ~/.config/ttr/.ttr.yaml
    let config_dir_config = dirs::config_dir()
        .map(|home| home.join("ttr").join(TTR_CONFIG))
        .filter(|config| config.is_file());
    if let Some(config) = config_dir_config {
        tasks.extend(tasks_from_file(config)?);
    }

    Ok(tasks)
}

fn create_process(task: &Task) -> Result<Child> {
    let current_dir = current_dir()?;
    let working_dir = task.working_dir.as_ref().unwrap_or(&current_dir);
    let child = Command::new("sh")
        .args(["-c", &format!("exec {}", task.cmd)])
        .current_dir(working_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

fn next_key_event() -> KeyEvent {
    let _raw = RawMode::enter();
    loop {
        let Ok(true) = event::poll(Duration::from_secs(60)) else {
            continue;
        };
        if let Event::Key(e) = event::read().expect("Unable to read event") {
            break e;
        }
    }
}

/// Presents a user with the list of tasks and reads the selected task
fn select_task<'a>(tasks: &'a [Task], status_line: &Option<String>) -> Result<Option<&'a Task>> {
    let _alt = AlternateScreen::enter();
    let mut stdout = stdout().lock();

    let mut error: Option<String> = None;
    loop {
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        println!();
        if let Some(status) = status_line {
            println!("    {}", status);
            println!();
        }
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
            println!("    Create file {} in the current directory", TTR_CONFIG);
        }
        println!();
        println!("    {} → {:10}", "q".stylize().red(), "quit");

        if let Some(e) = error.take() {
            println!("\n   {}\n", e.stylize().red());
        }

        let KeyEvent {
            code, modifiers, ..
        } = next_key_event();
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
