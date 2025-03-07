use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::Stylize,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env::current_dir,
    fs::File,
    io::stdout,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicI32, Ordering},
        Arc,
    },
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
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    clear_env: bool,
}

#[derive(Deserialize, Debug, Default)]
struct Group {
    name: String,
    key: char,
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default)]
    tasks: Vec<Task>,
}

impl Group {
    /// Iterates over all tasks and groups recursively
    ///
    /// Returns iterator over tuple of [`TaskOrGroup`] and path from the root
    /// to the element in an [`Vec`] form
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Task> {
        TaskIterator {
            tasks: vec![],
            groups: vec![self],
        }
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.groups.is_empty()
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

            let group = self.groups.pop()?;
            self.tasks.extend(group.tasks.iter_mut());
            self.groups.extend(group.groups.iter_mut());
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
        execute!(stdout(), EnterAlternateScreen, cursor::Hide)
            .expect("Unable to enter alternative screen");
        Self
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        // No need to unpack Result. We can't do anything about it anyway
        let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
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
    let tasks = merge_groups(read_tasks()?);

    let running_pid = Arc::new(AtomicI32::new(0));
    {
        let running_pid = Arc::clone(&running_pid);
        ctrlc::set_handler(move || {
            // pid values are not dependent on each other. Therefore relaxed is enough
            let pid = running_pid.load(Ordering::Relaxed);
            if pid > 0 {
                signal::kill(Pid::from_raw(pid), Signal::SIGINT).unwrap()
            }
        })?;
    }

    let mut status_line: Option<String> = None;
    'select_loop: loop {
        let Some(task) = select_task(&tasks, &status_line)? else {
            return Ok(());
        };

        'task_loop: loop {
            if task.clear || opts.clear {
                execute!(stdout(), Clear(ClearType::All), cursor::MoveTo(0, 0))?;
            }
            let mut process = create_process(task, true)?;

            running_pid.store(process.id() as i32, Ordering::Relaxed);
            let exit_status = process.wait()?;
            running_pid.store(0, Ordering::Relaxed);

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
fn merge_groups(groups: Vec<Group>) -> Group {
    let mut tasks: HashMap<char, Task> = HashMap::new();
    let mut similar_groups: HashMap<char, Vec<Group>> = HashMap::new();
    let Some(first_group) = groups.first() else {
        return Group::default();
    };
    let group_name = first_group.name.clone();
    let group_key = first_group.key;
    let mut groups = groups
        .into_iter()
        .filter(|g| g.name == group_name)
        .filter(|g| g.key == group_key)
        .collect::<Vec<_>>();
    if groups.len() == 1 {
        return groups.swap_remove(0);
    }
    for group in groups.into_iter() {
        for child_group in group.groups.into_iter() {
            similar_groups
                .entry(child_group.key)
                .or_default()
                .push(child_group)
        }

        for task in group.tasks.into_iter() {
            if similar_groups.contains_key(&task.key) {
                // key is already binded to a group
                continue;
            }
            tasks.entry(task.key).or_insert(task);
        }
    }

    let merged_groups = similar_groups
        .into_values()
        .map(merge_groups)
        .collect::<Vec<_>>();
    let merged_tasks = tasks.into_values().collect::<Vec<_>>();

    Group {
        name: group_name,
        key: group_key,
        groups: merged_groups,
        tasks: merged_tasks,
    }
}

fn read_tasks() -> Result<Vec<Group>> {
    // Basically mirror [`Group`] struct without some arguments meaningless for the root group
    #[derive(Deserialize)]
    struct Root {
        groups: Option<Vec<Group>>,
        tasks: Option<Vec<Task>>,
    }
    fn tasks_from_file(path: impl AsRef<Path>) -> Result<Group> {
        let file = File::open(path.as_ref())?;
        let config: Root = serde_yaml::from_reader(file)?;
        let tasks = config.tasks.unwrap_or_default();
        let groups = config.groups.unwrap_or_default();
        let key = '_';
        let name = "ROOT".to_string();
        let mut config = Group {
            tasks,
            groups,
            name,
            key,
        };
        // working directories if provided interpreted as relative to the file they are defined in
        let context_dir = path.as_ref().parent();
        for task in config.iter_mut() {
            if let Some(working_dir) = &task.working_dir {
                task.working_dir = context_dir.map(|p| p.join(working_dir));
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
            tasks.push(tasks_from_file(config)?);
        }
        dir = d.parent()
    }

    // ~/.ttr.yaml
    let home_dir_config = dirs::home_dir()
        .map(|home| home.join(TTR_CONFIG))
        .filter(|config| config.is_file());
    if let Some(config) = home_dir_config {
        tasks.push(tasks_from_file(config)?);
    }

    // ~/.config/ttr/.ttr.yaml
    let config_dir_config = dirs::config_dir()
        .map(|home| home.join("ttr").join(TTR_CONFIG))
        .filter(|config| config.is_file());
    if let Some(config) = config_dir_config {
        tasks.push(tasks_from_file(config)?);
    }

    Ok(tasks)
}

fn create_process(task: &Task, inherit_stdio: bool) -> Result<Child> {
    let current_dir = current_dir()?;
    let working_dir = task.working_dir.as_ref().unwrap_or(&current_dir);
    let mut child = Command::new("sh");
    child
        .args(["-c", &format!("exec {}", task.cmd)])
        .current_dir(working_dir)
        .stdin(if inherit_stdio {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .stdout(if inherit_stdio {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .stderr(if inherit_stdio {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });

    if task.clear_env {
        child.env_clear();
    }

    child.envs(&task.env);

    Ok(child.spawn()?)
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

enum DrawItem<'a> {
    Task(&'a Task),
    Group(&'a Group),
}

impl<'a> DrawItem<'a> {
    fn key(&self) -> char {
        match self {
            DrawItem::Group(g) => g.key,
            DrawItem::Task(t) => t.key,
        }
    }

    fn name(&'a self) -> &'a str {
        match self {
            DrawItem::Group(g) => &g.name,
            DrawItem::Task(t) => &t.name,
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
        execute!(stdout, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
        println!();
        if let Some(status) = status_line {
            println!("  {}", status);
            println!();
        }
        let current_group = *stack.last().unwrap();
        if !current_group.is_empty() {
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

            draw_tasks(current_group)?;
        } else {
            println!("    {}", "No tasks configured".stylize().bold());
            println!("    Create file {} in the current directory", TTR_CONFIG);
        }
        println!();
        println!("    {} → {:12}", "q".stylize().red(), "quit");
        if stack.len() > 1 {
            println!(" {} → {:12}", "<BS>".stylize().red(), "up");
        }

        if let Some(e) = error.take() {
            println!();
            println!("   {}", e.stylize().red());
            println!();
        }

        let KeyEvent {
            code, modifiers, ..
        } = next_key_event();
        let reason = match code {
            KeyCode::Char('q') => return Ok(None),
            KeyCode::Char(' ') => "Whitespace is not allowed".to_string(),
            KeyCode::Backspace | KeyCode::Esc if stack.len() <= 1 => "This is the root".to_string(),
            KeyCode::Backspace | KeyCode::Esc if stack.len() > 1 => {
                stack.pop();
                continue;
            }
            KeyCode::Char(ch) if modifiers != KeyModifiers::CONTROL => {
                let task = current_group.tasks.iter().find(|t| t.key == ch);
                if let Some(task) = task {
                    return Ok(Some(task));
                }
                let next_group = current_group.groups.iter().find(|g| g.key == ch);
                if let Some(next_group) = next_group {
                    stack.push(next_group);
                    continue;
                }
                format!("No task for key: {}", ch)
            }
            _ => "Please enter character key".to_string(),
        };
        error = Some(reason)
    }
}

fn draw_tasks(group: &Group) -> Result<()> {
    let groups = group.groups.iter().map(DrawItem::Group);
    let tasks = group.tasks.iter().map(DrawItem::Task);
    let draw_items = Vec::from_iter(groups.chain(tasks));

    let (width, _) = crossterm::terminal::size()?;
    // 4 characters is a padding from screen edge
    // 20 is width of one task representation
    let columns_fit = (width as usize - 4) / 20;
    let rows = draw_items.len().div_ceil(columns_fit);
    let columns = draw_items.chunks(rows).collect::<Vec<_>>();
    for i in 0..rows {
        print!("  ");
        for column in &columns {
            let Some(item) = column.get(i) else {
                break;
            };
            let name = if item.name().len() > 12 {
                format!("{}…", item.name().chars().take(11).collect::<String>())
            } else {
                item.name().to_string()
            };
            let key = item.key().stylize().bold();
            let key = if let DrawItem::Group(_) = item {
                key.dark_blue()
            } else {
                key.green()
            };
            print!(" {key} → {name:12}  ", key = key, name = name);
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::{remove_var, set_var};

    #[test]
    fn check_yaml_serialization() {
        let yaml = "
            name: name
            key: c
            groups:
            - name: foo
              key: f
              tasks:
              - name: foo
                cmd: foo
                key: b
        ";
        let group: Group = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(1, group.groups.len());
    }

    #[test]
    fn check_iteration() {
        let yaml = "
            name: name
            key: c
            groups:
            - name: foo
              key: f
              tasks:
              - name: bar
                cmd: --
                key: b
            - name: boo
              key: u
              tasks:
              - name: boo
                key: o
                cmd: '--'
        ";
        let mut group: Group = serde_yaml::from_str(yaml).unwrap();
        let names: Vec<_> = group.iter_mut().map(|s| s.name.as_str()).collect();
        assert_eq!(vec!["boo", "bar"], names);
    }

    #[test]
    fn check_env_config_without_clear() {
        let _env_var = session_env_var("GLOBAL_VAR_123", "present");

        let task = Task {
            name: "bar".to_string(),
            key: 'b',
            cmd: "echo -n \"The value of FOO is $FOO and GLOBAL_VAR_123 is $GLOBAL_VAR_123\""
                .to_string(),
            confirm: false,
            clear: false,
            working_dir: None,
            env: [("FOO".to_string(), "bar".to_string())]
                .iter()
                .cloned()
                .collect(),
            clear_env: false,
        };

        let output = create_process(&task, false)
            .unwrap()
            .wait_with_output()
            .unwrap();

        assert_eq!(
            "The value of FOO is bar and GLOBAL_VAR_123 is present",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn check_env_config_with_clear() {
        let _env_var = session_env_var("GLOBAL_VAR_234", "global");

        let task = Task {
            name: "bar".to_string(),
            key: 'b',
            cmd: "echo -n \"The value of FOO is $FOO and GLOBAL_VAR_234 is $GLOBAL_VAR_234\""
                .to_string(),
            confirm: false,
            clear: false,
            working_dir: None,
            env: [("FOO".to_string(), "bar".to_string())]
                .iter()
                .cloned()
                .collect(),
            clear_env: true,
        };

        let output = create_process(&task, false)
            .unwrap()
            .wait_with_output()
            .unwrap();

        assert_eq!(
            "The value of FOO is bar and GLOBAL_VAR_234 is ",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    fn session_env_var(name: impl Into<String>, value: impl Into<String>) -> EnvVar {
        let name = name.into();
        let value = value.into();

        set_var(&name, value.as_str());

        EnvVar { name }
    }

    struct EnvVar {
        name: String,
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            remove_var(&self.name)
        }
    }
}
