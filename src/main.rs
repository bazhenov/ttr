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
    fs::File,
    io::stdout,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const TTR_CONFIG: &str = ".ttr.yaml";

#[derive(Deserialize, Debug)]
struct Task {
    name: String,
    key: char,
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    confirm: bool,
}

struct AlternateScreen;

impl AlternateScreen {
    fn enter() -> Self {
        execute!(stdout(), EnterAlternateScreen).unwrap();
        Self
    }
}

impl Drop for AlternateScreen {
    fn drop(&mut self) {
        execute!(stdout(), LeaveAlternateScreen).unwrap();
    }
}

fn main() {
    let config = read_tasks();
    let Some(task) = select_task(&config) else {
        return
    };
    let exit_status = create_process(task).wait().expect("Process failed");

    if task.confirm || !exit_status.success() {
        println!();
        if exit_status.success() {
            println!(
                "   Task {}. Press <Enter> to continue...",
                "completed".stylize().green().bold(),
            );
        } else {
            println!(
                "   Task {} ({}). Press <Enter> to continue...",
                "failed".stylize().red().bold(),
                exit_status,
            );
        };
        while read_key_code() != KeyCode::Enter {}
    }
}

fn read_tasks() -> Vec<Task> {
    let mut tasks = vec![];
    if let Some(config) = user_config() {
        tasks.extend(read_tasks_from_file(config));
    }
    if let Some(config) = cwd_config() {
        tasks.extend(read_tasks_from_file(config));
    }
    tasks
}

fn read_tasks_from_file(path: impl AsRef<Path>) -> Vec<Task> {
    let file = File::open(path).unwrap();
    serde_yaml::from_reader(file).unwrap()
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
    Command::new(task.cmd.clone())
        .args(task.args.clone())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Unable to start")
}

fn read_key_code() -> KeyCode {
    let KeyEvent { code, .. } = read_key_event();
    code
}

fn read_key_event() -> KeyEvent {
    enable_raw_mode().unwrap();
    let key_code = loop {
        if let Ok(true) = event::poll(Duration::from_secs(60)) {
            let event = event::read().unwrap();
            if let Event::Key(e) = event {
                break e;
            }
        }
    };
    disable_raw_mode().unwrap();
    key_code
}

fn select_task(tasks: &[Task]) -> Option<&Task> {
    let _alt = AlternateScreen::enter();
    let mut stdout = stdout().lock();

    let mut error: Option<String> = None;
    loop {
        execute!(stdout, Clear(ClearType::All), MoveTo(0, 0)).unwrap();
        println!();
        if !tasks.is_empty() {
            println!("    {}", "SELECT A TASK".stylize().grey());
            println!();
            for task in tasks {
                println!(
                    "    {} → {:10}",
                    task.key.stylize().green().bold(),
                    task.name.clone().stylize().white()
                );
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
        } = read_key_event();
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
            Ok(task) => return task,
            Err(reason) => error = Some(reason),
        };
    }
}
