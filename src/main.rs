use crossterm::{
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
    process::{Child, Command, Stdio},
    time::Duration,
};

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
    let file = File::open("./tasks.yaml").unwrap();
    let yaml: Vec<Task> = serde_yaml::from_reader(file).unwrap();

    let Some(task) = select_task(&yaml) else {
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
        execute!(stdout, Clear(ClearType::All)).unwrap();
        println!("    {}", "SELECT A TASK".stylize().grey());
        println!();
        for task in tasks {
            println!(
                "    <{}>  {:10}",
                task.key.stylize().green().bold(),
                task.name.clone().stylize().white().bold()
            );
        }
        println!();
        println!("    <{}>  {:10}", "q".stylize().red(), "quit");

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
