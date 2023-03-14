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

#[derive(Deserialize, Debug)]
struct Group {
    name: String,
    key: char,
    children: Vec<TaskOrGroup>,
}

#[derive(Deserialize, Debug)]
enum TaskOrGroup {
    Task(Task),
    Group(Group),
}

impl TaskOrGroup {
    fn key(&self) -> char {
        match self {
            Self::Task(t) => t.key,
            Self::Group(g) => g.key,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Task(t) => &t.name,
            Self::Group(g) => &g.name,
        }
    }

    /// Iterates over all tasks and groups recursively
    ///
    /// Returns iterator over tuple of [`TaskOrGroup`] and path from the root
    /// to the element in an [`Vec`] form
    fn iter_mut<'a>(&'a mut self) -> impl Iterator<Item = &'a mut Task> {
        match self {
            TaskOrGroup::Group(g) => TaskIterator {
                tasks: vec![],
                groups: vec![g],
            },
            TaskOrGroup::Task(t) => TaskIterator {
                tasks: vec![t],
                groups: vec![],
            },
        }
    }
}

struct TaskIterator<'a> {
    groups: Vec<&'a mut Group>,
    tasks: Vec<&'a mut Task>,
}

impl<'a> Iterator for TaskIterator<'a> {
    type Item = &'a mut Task;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(task) = self.tasks.pop() {
                return Some(task);
            }

            let Some(group) = self.groups.pop() else {
                return None;
            };
            for child in group.children.iter_mut() {
                match child {
                    TaskOrGroup::Group(g) => self.groups.push(g),
                    TaskOrGroup::Task(t) => self.tasks.push(t),
                }
            }
            continue;
        }
    }
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
    tasks.sort_by(|a, b| a.name().cmp(&b.name()));

    let mut status_line: Option<String> = None;
    let root = Group {
        name: "ROOT".to_string(),
        key: 'r',
        children: tasks,
    };
    'select_loop: loop {
        let Some(task) = select_task(&root, &status_line)? else {
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
fn deduplicate_tasks(tasks: Vec<TaskOrGroup>) -> Vec<TaskOrGroup> {
    let mut duplicates = HashSet::new();
    tasks
        .into_iter()
        .filter(|t| duplicates.insert(t.key()))
        .collect()
}

fn read_tasks() -> Result<Vec<TaskOrGroup>> {
    fn tasks_from_file(path: impl AsRef<Path>) -> Result<Vec<TaskOrGroup>> {
        let file = File::open(path.as_ref())?;
        let mut config: Vec<TaskOrGroup> = serde_yaml::from_reader(file)?;

        // working directories if provided interpreted as relative to the file they are defined in
        let context_dir = path.as_ref().parent();
        for task in config.iter_mut() {
            for t in task.iter_mut() {
                if let Some(working_dir) = &t.working_dir {
                    t.working_dir = context_dir.map(|p| p.join(working_dir));
                }
            }
        }
        Ok(config)
    }

    let mut tasks = vec![];

    let stop_dir = dirs::home_dir().unwrap_or(PathBuf::from("/"));
    let start_dir = current_dir()?;
    let mut dir = Some(start_dir.as_path());

    while let Some(d) = dir {
        if d == stop_dir {
            break;
        }
        let config = d.join(TTR_CONFIG);
        if config.is_file() {
            tasks.extend(tasks_from_file(config)?);
        }
        dir = d.parent()
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
fn select_task<'a>(group: &'a Group, status_line: &Option<String>) -> Result<Option<&'a Task>> {
    let mut stack = vec![group];
    let _alt = AlternateScreen::enter();
    let mut stdout = stdout().lock();

    let mut error: Option<String> = None;
    loop {
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
        println!();
        if let Some(status) = status_line {
            println!("  {}", status);
            println!();
        }
        let tasks = &stack.last().unwrap().children;
        if !tasks.is_empty() {
            print!("  {}", "SELECT A TASK".stylize().grey());
            if stack.len() > 1 {
                let breadcrumbs = stack[1..]
                    .iter()
                    .map(|g| g.name.as_str())
                    .collect::<Vec<_>>()
                    .join(" → ");
                print!(" → {}", breadcrumbs);
            }
            println!();
            println!();

            draw_tasks(tasks)?;
        } else {
            println!("    {}", "No tasks configured".stylize().bold());
            println!("    Create file {} in the current directory", TTR_CONFIG);
        }
        println!();
        println!("    {} → {:12}", "q".stylize().red(), "quit");
        if stack.len() > 1 {
            println!("    {} → {:12}", "<BS>".stylize().red(), "up");
        }

        if let Some(e) = error.take() {
            println!();
            println!("   {}", e.stylize().red());
            println!();
        }

        let KeyEvent {
            code, modifiers, ..
        } = next_key_event();
        let task = match code {
            KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('c') if modifiers == KeyModifiers::CONTROL => return Ok(None),
            KeyCode::Char(' ') => Err("Whitespace is not allowed".to_string()),
            KeyCode::Backspace | KeyCode::Esc if stack.len() <= 1 => {
                Err("This is the root".to_string())
            }
            KeyCode::Backspace | KeyCode::Esc if stack.len() > 1 => {
                stack.pop();
                continue;
            }
            KeyCode::Char(ch) => tasks
                .iter()
                .find(|t| t.key() == ch)
                .ok_or(format!("No task for key: {}", ch)),
            _ => Err("Please enter character key".to_string()),
        };
        match task {
            Ok(TaskOrGroup::Task(task)) => return Ok(Some(task)),
            Ok(TaskOrGroup::Group(group)) => {
                stack.push(group);
                continue;
            }
            Err(reason) => error = Some(reason),
        };
    }
}

fn draw_tasks(tasks: &Vec<TaskOrGroup>) -> Result<()> {
    let (width, _) = crossterm::terminal::size()?;
    // 4 characters is a padding from screen edge
    // 20 is width of one task representation
    let columns_fit = (width as usize - 4) / 20;
    let rows = (tasks.len() + columns_fit - 1) / columns_fit;
    let columns = tasks.chunks(rows).collect::<Vec<_>>();
    Ok(for i in 0..rows {
        print!("  ");
        for column in &columns {
            let Some(task) = column.get(i) else {
                break;
            };
            let name = if task.name().len() > 12 {
                format!("{}…", task.name().chars().take(11).collect::<String>())
            } else {
                task.name().to_string()
            };
            print!("  {} → {:12}  ", task.key().stylize().green().bold(), name);
        }
        println!();
    })
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn check_yaml_serialization() {
        let yaml = "!Group
            name: foo
            key: f
            children:
            - !Task
                name: foo
                cmd: foo
                key: b
        ";
        let group: TaskOrGroup = serde_yaml::from_str(yaml).unwrap();
        let TaskOrGroup::Group(g) = group else {
            panic!("No group found");
        };
        assert_eq!(1, g.children.len());
    }

    #[test]
    fn check_iteration() {
        let yaml = "!Group
            name: foo
            key: f
            children:
            - !Task
                name: bar
                cmd: --
                key: b
            - !Group
                name: baz
                key: u
                children:
                - !Task
                    name: boo
                    key: o
                    cmd: --
        ";
        let mut group: TaskOrGroup = serde_yaml::from_str(yaml).unwrap();
        let names: Vec<_> = group.iter_mut().map(|s| s.name.as_str()).collect();
        assert_eq!(vec!["bar", "boo"], names);
    }
}
