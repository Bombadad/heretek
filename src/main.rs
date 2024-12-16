use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, thread};
use std::{error::Error, io};

use clap::Parser;
use deku::ctx::Endian;
use env_logger::{Builder, Env};
use gdb::write_mi;
use log::{debug, error};
use ratatui::crossterm::{
    event::{self, DisableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::ScrollbarState;
use regex::Regex;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use mi::{data_read_memory_bytes, Asm, MemoryMapping, Register};

mod gdb;
mod mi;
mod ui;

enum InputMode {
    Normal,
    Editing,
}

use std::collections::{HashMap, VecDeque};

fn resolve_home(path: &str) -> Option<PathBuf> {
    if path.starts_with("~/") {
        if let Ok(home) = env::var("HOME") {
            return Some(Path::new(&home).join(&path[2..]));
        }
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[derive(Debug, Clone)]
struct LimitedBuffer<T> {
    offset: usize,
    buffer: VecDeque<T>,
    capacity: usize,
}

impl<T> LimitedBuffer<T> {
    fn as_slice(&self) -> &[T] {
        self.buffer.as_slices().0
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    fn len(&self) -> usize {
        self.buffer.len()
    }

    fn new(capacity: usize) -> Self {
        Self { offset: 0, buffer: VecDeque::with_capacity(capacity), capacity }
    }

    fn push(&mut self, value: T) {
        if self.buffer.len() == self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(value);
    }
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Run gdb as child process from PATH
    #[arg(short, long)]
    local: bool,

    /// Connect to nc session
    ///
    /// `mkfifo gdb_sock; cat gdb_pipe | gdb --interpreter=mi | nc -l -p 12345 > gdb_pipe`
    #[arg(short, long)]
    remote: Option<SocketAddr>,

    /// Switch into 32-bit mode
    #[arg(long = "32")]
    thirty_two_bit: bool,
}

enum Mode {
    All,
    OnlyRegister,
    OnlyStack,
    OnlyInstructions,
    OnlyOutput,
    OnlyMapping,
    OnlyHexdump,
}

impl Mode {
    pub fn next(&self) -> Self {
        match self {
            Mode::All => Mode::OnlyRegister,
            Mode::OnlyRegister => Mode::OnlyStack,
            Mode::OnlyStack => Mode::OnlyInstructions,
            Mode::OnlyInstructions => Mode::OnlyOutput,
            Mode::OnlyOutput => Mode::OnlyMapping,
            Mode::OnlyMapping => Mode::OnlyHexdump,
            Mode::OnlyHexdump => Mode::All,
        }
    }
}

// TODO: this could be split up, some of these fields
// are always set after the file is loaded in gdb
struct App {
    next_write: Arc<Mutex<Vec<String>>>,
    written: Arc<Mutex<VecDeque<Written>>>,
    thirty_two_bit: Arc<Mutex<bool>>,
    filepath: Arc<Mutex<Option<PathBuf>>>,
    endian: Arc<Mutex<Option<Endian>>>,
    mode: Mode,
    input: Input,
    input_mode: InputMode,
    sent_input: LimitedBuffer<String>,
    memory_map: Arc<Mutex<Option<Vec<MemoryMapping>>>>,
    memory_map_scroll: usize,
    memory_map_scroll_state: ScrollbarState,
    current_pc: Arc<Mutex<u64>>, // TODO: replace with AtomicU64?
    output: Arc<Mutex<Vec<String>>>,
    output_scroll: usize,
    output_scroll_state: ScrollbarState,
    stream_output_prompt: Arc<Mutex<String>>,
    gdb_stdin: Arc<Mutex<dyn Write + Send>>,
    register_changed: Arc<Mutex<Vec<u8>>>,
    register_names: Arc<Mutex<Vec<String>>>,
    registers: Arc<Mutex<Vec<(String, Option<Register>, Vec<u64>)>>>,
    stack: Arc<Mutex<HashMap<u64, Vec<u64>>>>,
    asm: Arc<Mutex<Vec<Asm>>>,
    hexdump: Arc<Mutex<Option<(u64, Vec<u8>)>>>,
    hexdump_scroll: usize,
    hexdump_scroll_state: ScrollbarState,
}

impl App {
    /// Create new stream to gdb
    /// - remote: Connect to gdb via a TCP connection
    /// - local: Connect to gdb via spawning a gdb process
    ///
    ///
    /// # Returns
    /// `(gdb_stdin, App)`
    pub fn new_stream(args: Args) -> (BufReader<Box<dyn Read + Send>>, App) {
        let (reader, gdb_stdin): (BufReader<Box<dyn Read + Send>>, Arc<Mutex<dyn Write + Send>>) =
            match (&args.local, &args.remote) {
                (true, None) => {
                    let mut gdb_process = Command::new("gdb")
                        .args(["--interpreter=mi2", "--quiet", "-nx"])
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .spawn()
                        .expect("Failed to start GDB");

                    let reader = BufReader::new(
                        Box::new(gdb_process.stdout.unwrap()) as Box<dyn Read + Send>
                    );
                    let gdb_stdin = gdb_process.stdin.take().unwrap();
                    let gdb_stdin = Arc::new(Mutex::new(gdb_stdin));

                    (reader, gdb_stdin)
                }
                (false, Some(remote)) => {
                    let tcp_stream = TcpStream::connect(remote).unwrap(); // Example address
                    let reader = BufReader::new(
                        Box::new(tcp_stream.try_clone().unwrap()) as Box<dyn Read + Send>
                    );
                    let gdb_stdin = Arc::new(Mutex::new(tcp_stream.try_clone().unwrap()));

                    (reader, gdb_stdin)
                }
                _ => panic!("Invalid configuration"),
            };

        let app = App {
            next_write: Arc::new(Mutex::new(vec![])),
            written: Arc::new(Mutex::new(VecDeque::new())),
            thirty_two_bit: Arc::new(Mutex::new(args.thirty_two_bit)),
            filepath: Arc::new(Mutex::new(None)),
            endian: Arc::new(Mutex::new(None)),
            mode: Mode::All,
            input: Input::default(),
            input_mode: InputMode::Normal,
            sent_input: LimitedBuffer::new(100),
            current_pc: Arc::new(Mutex::new(0)),
            output_scroll: 0,
            output_scroll_state: ScrollbarState::new(0),
            memory_map: Arc::new(Mutex::new(None)),
            memory_map_scroll: 0,
            memory_map_scroll_state: ScrollbarState::new(0),
            output: Arc::new(Mutex::new(Vec::new())),
            stream_output_prompt: Arc::new(Mutex::new(String::new())),
            register_changed: Arc::new(Mutex::new(vec![])),
            register_names: Arc::new(Mutex::new(vec![])),
            gdb_stdin,
            registers: Arc::new(Mutex::new(vec![])),
            stack: Arc::new(Mutex::new(HashMap::new())),
            asm: Arc::new(Mutex::new(Vec::new())),
            hexdump: Arc::new(Mutex::new(None)),
            hexdump_scroll: 0,
            hexdump_scroll_state: ScrollbarState::new(0),
        };

        (reader, app)
    }

    // Parse a "file filepath" command and save
    fn save_filepath(&mut self, val: &str) {
        let filepath: Vec<&str> = val.split_whitespace().collect();
        let filepath = resolve_home(filepath[1]).unwrap();
        // debug!("filepath: {filepath:?}");
        self.filepath = Arc::new(Mutex::new(Some(filepath)));
    }

    pub fn classify_val(&self, val: u64, filepath: &std::borrow::Cow<str>) -> (bool, bool, bool) {
        let mut is_stack = false;
        let mut is_heap = false;
        let mut is_text = false;
        if val != 0 {
            // look through, add see if the value is part of the stack
            let memory_map = self.memory_map.lock().unwrap();
            // trace!("{:02x?}", memory_map);
            if memory_map.is_some() {
                for r in memory_map.as_ref().unwrap() {
                    if r.contains(val) {
                        if r.is_stack() {
                            is_stack = true;
                            break;
                        } else if r.is_heap() {
                            is_heap = true;
                            break;
                        } else if r.is_path(filepath) {
                            // TODO(23): This could be expanded to all segments loaded in
                            // as executable
                            is_text = true;
                            break;
                        }
                    }
                }
            }
        }
        (is_stack, is_heap, is_text)
    }
}

#[derive(Debug)]
enum Written {
    /// Requested Register Value deref
    // TODO: Could this just be the register name?
    RegisterValue((String, u64)),
    /// Requested Stack Bytes
    ///
    /// None - This is the first time this is requested
    /// Some - This has alrady been read, and this is a deref, trust
    ///        the base_reg of .0
    Stack(Option<String>),
    /// Requested Memory Read (for hexdump)
    Memory,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    // Configure logging to a file
    let log_file = Arc::new(Mutex::new(File::create("app.log")?));
    Builder::from_env(Env::default().default_filter_or("debug"))
        .format(move |buf, record| {
            let mut log_file = log_file.lock().unwrap();
            let log_msg = format!(
                "{} [{}] - {}\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.args()
            );
            log_file.write_all(log_msg.as_bytes()).unwrap();
            writeln!(buf, "{}", log_msg.trim_end())
        })
        .target(env_logger::Target::Pipe(Box::new(std::io::sink()))) // Disable stdout/stderr
        .init();

    // Start rx thread
    let (gdb_stdout, mut app) = App::new_stream(args);

    // Setup terminal
    let mut terminal = ratatui::init();

    let filepath_arc = Arc::clone(&app.filepath);
    let thirty_two_bit_arc = Arc::clone(&app.thirty_two_bit);
    let next_write_arc = Arc::clone(&app.next_write);
    let written_arc = Arc::clone(&app.written);
    let endian_arc = Arc::clone(&app.endian);
    let gdb_stdin_arc = Arc::clone(&app.gdb_stdin);
    let current_pc_arc = Arc::clone(&app.current_pc);
    let output_arc = Arc::clone(&app.output);
    let stream_output_prompt_arc = Arc::clone(&app.stream_output_prompt);
    let register_changed_arc = Arc::clone(&app.register_changed);
    let register_names_arc = Arc::clone(&app.register_names);
    let registers_arc = Arc::clone(&app.registers);
    let memory_map_arc = Arc::clone(&app.memory_map);
    let stack_arc = Arc::clone(&app.stack);
    let asm_arc = Arc::clone(&app.asm);
    let hexdump_arc = Arc::clone(&app.hexdump);

    // Thread to read GDB output and parse it
    thread::spawn(move || {
        gdb::gdb_interact(
            gdb_stdout,
            next_write_arc,
            written_arc,
            thirty_two_bit_arc,
            endian_arc,
            filepath_arc,
            register_changed_arc,
            register_names_arc,
            registers_arc,
            current_pc_arc,
            stack_arc,
            asm_arc,
            gdb_stdin_arc,
            output_arc,
            stream_output_prompt_arc,
            memory_map_arc,
            hexdump_arc,
        )
    });

    // Run tui application
    let res = run_app(&mut terminal, &mut app);

    // restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err)
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::ui(f, app))?;

        // check and see if we need to write to GBD MI
        {
            let mut next_write = app.next_write.lock().unwrap();
            if !next_write.is_empty() {
                for w in &*next_write {
                    write_mi(&app.gdb_stdin, &w);
                }
                next_write.clear();
            }
        }

        if crossterm::event::poll(Duration::from_millis(10))? {
            if let Event::Key(key) = event::read()? {
                match (&app.input_mode, key.code, &app.mode) {
                    (InputMode::Normal, KeyCode::Char('i'), _) => {
                        app.input_mode = InputMode::Editing;
                    }
                    (InputMode::Normal, KeyCode::Char('q'), _) => {
                        return Ok(());
                    }
                    (InputMode::Normal, KeyCode::Tab, _) => {
                        app.mode = app.mode.next();
                    }
                    (_, KeyCode::F(1), _) => {
                        app.mode = Mode::All;
                    }
                    (_, KeyCode::F(2), _) => {
                        app.mode = Mode::OnlyRegister;
                    }
                    (_, KeyCode::F(3), _) => {
                        app.mode = Mode::OnlyStack;
                    }
                    (_, KeyCode::F(4), _) => {
                        app.mode = Mode::OnlyInstructions;
                    }
                    (_, KeyCode::F(5), _) => {
                        app.mode = Mode::OnlyOutput;
                    }
                    (_, KeyCode::F(6), _) => {
                        app.mode = Mode::OnlyMapping;
                    }
                    (_, KeyCode::F(7), _) => {
                        app.mode = Mode::OnlyHexdump;
                    }
                    (InputMode::Editing, KeyCode::Esc, _) => {
                        app.input_mode = InputMode::Normal;
                    }
                    // memory output
                    (InputMode::Normal, KeyCode::Char('j'), Mode::OnlyOutput) => {
                        let output_lock = app.output.lock().unwrap();
                        let len = output_lock.len();
                        scroll_down(1, &mut app.output_scroll, &mut app.output_scroll_state, len);
                    }
                    (InputMode::Normal, KeyCode::Char('k'), Mode::OnlyOutput) => {
                        scroll_up(1, &mut app.output_scroll, &mut app.output_scroll_state);
                    }
                    (InputMode::Normal, KeyCode::Char('J'), Mode::OnlyOutput) => {
                        let output_lock = app.output.lock().unwrap();
                        let len = output_lock.len();
                        scroll_down(50, &mut app.output_scroll, &mut app.output_scroll_state, len);
                    }
                    (InputMode::Normal, KeyCode::Char('K'), Mode::OnlyOutput) => {
                        scroll_up(50, &mut app.output_scroll, &mut app.output_scroll_state);
                    }
                    // memory mapping
                    (InputMode::Normal, KeyCode::Char('j'), Mode::OnlyMapping) => {
                        let memory_lock = app.memory_map.lock().unwrap();
                        if let Some(memory) = memory_lock.as_ref() {
                            let len = memory.len();
                            scroll_down(
                                1,
                                &mut app.memory_map_scroll,
                                &mut app.memory_map_scroll_state,
                                len,
                            );
                        }
                    }
                    (InputMode::Normal, KeyCode::Char('k'), Mode::OnlyMapping) => {
                        scroll_up(1, &mut app.memory_map_scroll, &mut app.memory_map_scroll_state);
                    }
                    (InputMode::Normal, KeyCode::Char('J'), Mode::OnlyMapping) => {
                        let memory_lock = app.memory_map.lock().unwrap();
                        if let Some(memory) = memory_lock.as_ref() {
                            let len = memory.len();
                            scroll_down(
                                50,
                                &mut app.memory_map_scroll,
                                &mut app.memory_map_scroll_state,
                                len,
                            );
                        }
                    }
                    (InputMode::Normal, KeyCode::Char('K'), Mode::OnlyMapping) => {
                        scroll_up(50, &mut app.memory_map_scroll, &mut app.memory_map_scroll_state);
                    }
                    // hexdump
                    (InputMode::Normal, KeyCode::Char('j'), Mode::OnlyHexdump) => {
                        let hexdump = app.hexdump.lock().unwrap();
                        if let Some(hexdump) = hexdump.as_ref() {
                            let len = hexdump.1.len();
                            scroll_down(
                                1,
                                &mut app.hexdump_scroll,
                                &mut app.hexdump_scroll_state,
                                len,
                            );
                        }
                    }
                    (InputMode::Normal, KeyCode::Char('k'), Mode::OnlyHexdump) => {
                        scroll_up(1, &mut app.hexdump_scroll, &mut app.hexdump_scroll_state);
                    }
                    (InputMode::Normal, KeyCode::Char('J'), Mode::OnlyHexdump) => {
                        let hexdump = app.hexdump.lock().unwrap();
                        if let Some(hexdump) = hexdump.as_ref() {
                            let len = hexdump.1.len();
                            scroll_down(
                                50,
                                &mut app.hexdump_scroll,
                                &mut app.hexdump_scroll_state,
                                len,
                            );
                        }
                    }
                    (InputMode::Normal, KeyCode::Char('K'), Mode::OnlyHexdump) => {
                        scroll_up(50, &mut app.hexdump_scroll, &mut app.hexdump_scroll_state);
                    }
                    (_, KeyCode::Enter, _) => {
                        key_enter(app)?;
                    }
                    (_, KeyCode::Down, _) => {
                        key_down(app);
                    }
                    (_, KeyCode::Up, _) => {
                        key_up(app);
                    }
                    (InputMode::Editing, _, _) => {
                        app.input.handle_event(&Event::Key(key));
                    }
                    _ => (),
                }
            }
        }
    }
}

fn scroll_down(n: usize, scroll: &mut usize, state: &mut ScrollbarState, len: usize) {
    if scroll < &mut len.saturating_sub(1) {
        *scroll += n;
        *state = state.position(*scroll);
    }
}

fn scroll_up(n: usize, scroll: &mut usize, state: &mut ScrollbarState) {
    if *scroll > n {
        *scroll -= n;
    } else {
        *scroll = 0;
    }
    *state = state.position(*scroll);
}

fn key_up(app: &mut App) {
    if !app.sent_input.buffer.is_empty() {
        if app.sent_input.offset < app.sent_input.buffer.len() {
            app.sent_input.offset += 1;
        }
        update_from_previous_input(app);
    } else {
        app.sent_input.offset = 0;
    }
}

fn key_down(app: &mut App) {
    if !app.sent_input.buffer.is_empty() {
        if app.sent_input.offset != 0 {
            app.sent_input.offset -= 1;
            if app.sent_input.offset == 0 {
                app.input.reset();
            }
        }
        update_from_previous_input(app);
    } else {
        app.sent_input.offset = 0;
    }
}

fn key_enter(app: &mut App) -> Result<(), io::Error> {
    if app.input.value().is_empty() {
        app.sent_input.offset = 0;

        let messages = app.sent_input.clone();
        let messages = messages.as_slice().iter();
        if let Some(val) = messages.last() {
            process_line(app, val);
        }
    } else {
        app.sent_input.offset = 0;
        app.sent_input.push(app.input.value().into());

        let val = app.input.clone();
        let val = val.value();
        process_line(app, val)
    }

    Ok(())
}

fn process_line(app: &mut App, val: &str) {
    let mut val = val.to_owned();

    // Replace internal variables
    replace_mapping_start(app, &mut val);
    replace_mapping_end(app, &mut val);

    if val.starts_with("file") {
        app.save_filepath(&val);
    } else if val.starts_with("hexdump") {
        let split: Vec<&str> = val.split_whitespace().collect();
        if split.len() < 3 {
            error!("Invalid arguments, expected 'hexdump addr len'");
            return;
        }
        let mut next_write = app.next_write.lock().unwrap();
        let mut written = app.written.lock().unwrap();
        let addr = split[1];
        let len = split[2];

        let addr_val = if addr.starts_with("0x") {
            u64::from_str_radix(&addr[2..], 16).unwrap()
        } else {
            addr.parse::<u64>().unwrap()
        };

        let len_val = if len.starts_with("0x") {
            u64::from_str_radix(&len[2..], 16).unwrap()
        } else {
            len.parse::<u64>().unwrap()
        };

        let s = data_read_memory_bytes(addr_val, 0, len_val);
        next_write.push(s);
        written.push_back(Written::Memory);
        app.input.reset();
        return;
    }
    gdb::write_mi(&app.gdb_stdin, &val);
    app.input.reset();
}

fn replace_mapping_start(app: &mut App, val: &mut String) {
    let memory_map = app.memory_map.lock().unwrap();
    if let Some(ref memory_map) = *memory_map {
        let pattern = Regex::new(r"\$HERETEK_MAPPING_START_([\w\[\]/.-]+)").unwrap();
        *val = pattern
            .replace_all(&*val, |caps: &regex::Captures| {
                let filename = &caps[1];
                format!(
                    "0x{:02x}",
                    memory_map
                        .iter()
                        // TODO(perf): to_owned
                        .find(|a| a.path == Some(filename.to_owned()))
                        .map(|a| a.start_address)
                        .unwrap_or(0)
                )
            })
            .to_string();
    }
}

fn replace_mapping_end(app: &mut App, val: &mut String) {
    let memory_map = app.memory_map.lock().unwrap();
    if let Some(ref memory_map) = *memory_map {
        let pattern = Regex::new(r"\$HERETEK_MAPPING_END_([\w\[\]/.-]+)").unwrap();
        *val = pattern
            .replace_all(&*val, |caps: &regex::Captures| {
                let filename = &caps[1];
                format!(
                    "0x{:02x}",
                    memory_map
                        .iter()
                        // TODO(perf): to_owned
                        .find(|a| a.path == Some(filename.to_owned()))
                        .map(|a| a.end_address)
                        .unwrap_or(0)
                )
            })
            .to_string();
    }
}

fn update_from_previous_input(app: &mut App) {
    if app.sent_input.buffer.len() >= app.sent_input.offset {
        if let Some(msg) =
            app.sent_input.buffer.get(app.sent_input.buffer.len() - app.sent_input.offset)
        {
            app.input = Input::new(msg.clone())
        }
    }
}
