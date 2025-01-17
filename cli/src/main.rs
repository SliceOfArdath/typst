mod args;
mod trace;

use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, BTreeMap};
use std::fs::{self, File};
use std::hash::Hash;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Datelike;
use clap::Parser;
use codespan_reporting::diagnostic::{Diagnostic, Label};
use codespan_reporting::term::{self, termcolor};
use comemo::{Prehashed, TrackedMut, Track};
use elsa::FrozenVec;
use memmap2::Mmap;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use same_file::{is_same_file, Handle};
use std::cell::OnceCell;
use termcolor::{ColorChoice, StandardStream, WriteColor};
use typst::diag::{bail, FileError, FileResult, SourceError, StrResult};
use typst::doc::Document;
use typst::eval::{Datetime, Library};
use typst::font::{Font, FontBook, FontInfo, FontVariant};
use typst::geom::Color;
use typst::syntax::{Source, SourceId};
use typst::util::{hash128, Access, AccessMode, Buffer, PathExt};
use typst::World;
use walkdir::WalkDir;

use crate::args::{CliArguments, Command, CompileCommand, DiagnosticFormat};

type CodespanResult<T> = Result<T, CodespanError>;
type CodespanError = codespan_reporting::files::Error;

thread_local! {
    static EXIT: Cell<ExitCode> = Cell::new(ExitCode::SUCCESS);
}

/// Entry point.
fn main() -> ExitCode {
    let arguments = CliArguments::parse();
    let _guard = match crate::trace::init_tracing(&arguments) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("failed to initialize tracing {}", err);
            None
        }
    };

    let res = match &arguments.command {
        Command::Compile(_) | Command::Watch(_) => {
            compile(CompileSettings::with_arguments(arguments))
        }
        Command::Fonts(_) => fonts(FontsSettings::with_arguments(arguments)),
    };

    if let Err(msg) = res {
        set_failed();
        print_error(&msg).expect("failed to print error");
    }

    EXIT.with(|cell| cell.get())
}

/// Ensure a failure exit code.
fn set_failed() {
    EXIT.with(|cell| cell.set(ExitCode::FAILURE));
}

/// Print an application-level error (independent from a source file).
fn print_error(msg: &str) -> io::Result<()> {
    let mut w = color_stream();
    let styles = term::Styles::default();

    w.set_color(&styles.header_error)?;
    write!(w, "error")?;

    w.reset()?;
    writeln!(w, ": {msg}.")
}

/// Used by `args.rs`.
fn typst_version() -> &'static str {
    env!("TYPST_VERSION")
}

/// A summary of the input arguments relevant to compilation.
struct CompileSettings {
    /// The path to the input file.
    input: PathBuf,
    /// The path to the output file.
    output: PathBuf,
    /// Whether to watch the input files for changes.
    watch: bool,
    /// The root directory for absolute paths.
    root: Option<PathBuf>,
    /// The destination directory for absolute paths.
    dest: Option<PathBuf>,
    /// The paths to search for fonts.
    font_paths: Vec<PathBuf>,
    /// The open command to use.
    open: Option<Option<String>>,
    /// The PPI to use for PNG export.
    ppi: Option<f32>,
    /// In which format to emit diagnostics.
    diagnostic_format: DiagnosticFormat,
}

impl CompileSettings {
    /// Create a new compile settings from the field values.
    #[allow(clippy::too_many_arguments)]
    fn new(
        input: PathBuf,
        output: Option<PathBuf>,
        watch: bool,
        root: Option<PathBuf>,
        dest: Option<PathBuf>,
        font_paths: Vec<PathBuf>,
        open: Option<Option<String>>,
        ppi: Option<f32>,
        diagnostic_format: DiagnosticFormat,
    ) -> Self {
        let output = match output {
            Some(path) => path,
            None => input.with_extension("pdf"),
        };
        Self {
            input,
            output,
            watch,
            root,
            dest,
            font_paths,
            open,
            diagnostic_format,
            ppi,
        }
    }

    /// Create a new compile settings from the CLI arguments and a compile command.
    ///
    /// # Panics
    /// Panics if the command is not a compile or watch command.
    fn with_arguments(args: CliArguments) -> Self {
        let watch = matches!(args.command, Command::Watch(_));
        let CompileCommand { input, output, open, ppi, diagnostic_format, .. } =
            match args.command {
                Command::Compile(command) => command,
                Command::Watch(command) => command,
                _ => unreachable!(),
            };

        Self::new(
            input,
            output,
            watch,
            args.root,
            args.dest,
            args.font_paths,
            open,
            ppi,
            diagnostic_format,
        )
    }
}

struct FontsSettings {
    /// The font paths
    font_paths: Vec<PathBuf>,
    /// Whether to include font variants
    variants: bool,
}

impl FontsSettings {
    /// Create font settings from the field values.
    fn new(font_paths: Vec<PathBuf>, variants: bool) -> Self {
        Self { font_paths, variants }
    }

    /// Create a new font settings from the CLI arguments.
    ///
    /// # Panics
    /// Panics if the command is not a fonts command.
    fn with_arguments(args: CliArguments) -> Self {
        match args.command {
            Command::Fonts(command) => Self::new(args.font_paths, command.variants),
            _ => unreachable!(),
        }
    }
}

/// Execute a compilation command.
fn compile(mut command: CompileSettings) -> StrResult<()> {
    // Determine the parent directory of the input file.
    let parent = command
        .input
        .canonicalize()
        .ok()
        .as_ref()
        .and_then(|path| path.parent())
        .unwrap_or(Path::new("."))
        .to_owned();
    let root = Ok(command.root.as_ref().unwrap_or(&parent).to_owned());
    let parent_dest = command
        .output
        .canonicalize()
        .ok()
        .as_ref()
        .and_then(|path| path.parent())
        .unwrap_or(Path::new("."))
        .to_owned();
    let dest = Ok(command.dest.as_ref().unwrap_or(&parent_dest.join("dest")).to_owned());

    //neither reading nor writing are disabled, by default, though they may be, if need be.
    let mut wp = WriteStorage::default();

    // Create the world that serves sources, fonts and files.
    let mut world = SystemWorld::new(root, dest, &command.font_paths, &mut wp);

    // Perform initial compilation.
    let ok = compile_once(&mut world, &command)?;

    // Open the file if requested, this must be done on the first **successful**
    // compilation.
    if ok {
        if let Some(open) = command.open.take() {
            open_file(open.as_deref(), &command.output)?;
        }
    }

    if !command.watch {
        return Ok(());
    }

    // Setup file watching.
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())
        .map_err(|_| "failed to watch directory")?;

    // Watch the input file's parent directory recursively.
    watcher
        .watch(&parent, RecursiveMode::Recursive)
        .map_err(|_| "failed to watch parent directory")?;

    // Watch the root directory recursively.
    if let Ok(root) = &world.root {
        if *root != parent {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .map_err(|_| "failed to watch root directory")?;
        }
    }
    // Unwatch the dest directory recursively.
    if let Ok(dest) = &world.dest {
        if *dest != parent {
            let _ = watcher.unwatch(dest); //we discard the result
        }
    }

    // Handle events.
    let timeout = std::time::Duration::from_millis(100);
    loop {
        let mut recompile = false;
        for event in rx
            .recv()
            .into_iter()
            .chain(std::iter::from_fn(|| rx.recv_timeout(timeout).ok()))
        {
            let event = event.map_err(|_| "failed to watch directory")?;
            if event
                .paths
                .iter()
                .all(|path| is_same_file(path, &command.output).unwrap_or(false))
            {
                continue;
            }

            recompile |= world.relevant(&event);
        }

        if recompile {
            let ok = compile_once(&mut world, &command)?;
            comemo::evict(30);

            // Ipen the file if requested, this must be done on the first
            // **successful** compilation
            if ok {
                if let Some(open) = command.open.take() {
                    open_file(open.as_deref(), &command.output)?;
                }
            }
        }
    }
}

/// Compile a single time.
///
/// Returns whether it compiled without errors.
#[tracing::instrument(skip_all)]
fn compile_once(world: &mut SystemWorld, command: &CompileSettings) -> StrResult<bool> {
    tracing::info!("Starting compilation");

    status(command, Status::Compiling).unwrap();

    world.reset();
    world.main = world.resolve(&command.input).map_err(|err| err.to_string())?;

    match typst::compile(world) {
        // Export the PDF / PNG.
        Ok(document) => {
            export(&document, command)?;
            write(world)?;
            status(command, Status::Success).unwrap();
            tracing::info!("Compilation succeeded");
            Ok(true)
        }

        // Print diagnostics.
        Err(errors) => {
            set_failed();
            status(command, Status::Error).unwrap();
            print_diagnostics(world, *errors, command.diagnostic_format)
                .map_err(|_| "failed to print diagnostics")?;
            tracing::info!("Compilation failed");
            Ok(false)
        }
    }
}

/// Export into the target format.
fn export(document: &Document, command: &CompileSettings) -> StrResult<()> {
    match command.output.extension() {
        Some(ext) if ext.eq_ignore_ascii_case("png") => {
            // Determine whether we have a `{n}` numbering.
            let string = command.output.to_str().unwrap_or_default();
            let numbered = string.contains("{n}");
            if !numbered && document.pages.len() > 1 {
                bail!("cannot export multiple PNGs without `{{n}}` in output path");
            }

            // Find a number width that accommodates all pages. For instance, the
            // first page should be numbered "001" if there are between 100 and
            // 999 pages.
            let width = 1 + document.pages.len().checked_ilog10().unwrap_or(0) as usize;
            let ppi = command.ppi.unwrap_or(2.0);
            let mut storage;

            for (i, frame) in document.pages.iter().enumerate() {
                let pixmap = typst::export::render(frame, ppi, Color::WHITE);
                let path = if numbered {
                    storage = string.replace("{n}", &format!("{:0width$}", i + 1));
                    Path::new(&storage)
                } else {
                    command.output.as_path()
                };
                pixmap.save_png(path).map_err(|_| "failed to write PNG file")?;
            }
        }
        _ => {
            let buffer = typst::export::pdf(document);
            fs::write(&command.output, buffer).map_err(|_| "failed to write PDF file")?;
        }
    }
    Ok(())
}

/// Apply write calls
/// These are very limited in where they can write, which is no issue as we excpect to be unable to write everywhere
#[tracing::instrument(skip_all)]
fn write(world: &SystemWorld) -> StrResult<()> {
    // Find file
    tracing::info!("Writing result files..");
    let hashes = world.hashes.borrow();
    for (h, s) in world.wpaths.dump() {
        let loc = hashes.iter().find(|(_, v)| match v {
            Err(_) => false,
            Ok(v) => *v == h,
        });
        if let Some((path, _)) = loc {
            let data = s;
            if data.is_empty() {
                // Nothing to write
                continue;
            } else {
                // Remember; we aren't interested with order conservation here! what's important is that the data is there.
                let buffer: Vec<u8> = data.dump();
                // Generate file name, and write
                tracing::info!(
                    "Writing file: {}",
                    path.to_str().unwrap_or("{invalid_name}")
                );
                fs::write(path, buffer).map_err(|_| {
                    format!(
                        "failed to write {} file",
                        path.file_name()
                            .map_or("..", |s| s.to_str().unwrap_or("{invalid_name}"))
                    )
                })?;
            }
        }
    }
    Ok(())
}

/// Clear the terminal and render the status message.
#[tracing::instrument(skip_all)]
fn status(command: &CompileSettings, status: Status) -> io::Result<()> {
    if !command.watch {
        return Ok(());
    }

    let esc = 27 as char;
    let input = command.input.display();
    let output = command.output.display();
    let time = chrono::offset::Local::now();
    let timestamp = time.format("%H:%M:%S");
    let message = status.message();
    let color = status.color();

    let mut w = color_stream();
    if std::io::stderr().is_terminal() {
        // Clear the terminal.
        write!(w, "{esc}c{esc}[1;1H")?;
    }

    w.set_color(&color)?;
    write!(w, "watching")?;
    w.reset()?;
    writeln!(w, " {input}")?;

    w.set_color(&color)?;
    write!(w, "writing to")?;
    w.reset()?;
    writeln!(w, " {output}")?;

    writeln!(w)?;
    writeln!(w, "[{timestamp}] {message}")?;
    writeln!(w)?;

    w.flush()
}

/// Get stderr with color support if desirable.
fn color_stream() -> termcolor::StandardStream {
    termcolor::StandardStream::stderr(if std::io::stderr().is_terminal() {
        ColorChoice::Auto
    } else {
        ColorChoice::Never
    })
}

/// The status in which the watcher can be.
enum Status {
    Compiling,
    Success,
    Error,
}

impl Status {
    fn message(&self) -> &str {
        match self {
            Self::Compiling => "compiling ...",
            Self::Success => "compiled successfully",
            Self::Error => "compiled with errors",
        }
    }

    fn color(&self) -> termcolor::ColorSpec {
        let styles = term::Styles::default();
        match self {
            Self::Error => styles.header_error,
            _ => styles.header_note,
        }
    }
}

/// Print diagnostic messages to the terminal.
fn print_diagnostics(
    world: &SystemWorld,
    errors: Vec<SourceError>,
    diagnostic_format: DiagnosticFormat,
) -> Result<(), codespan_reporting::files::Error> {
    let mut w = match diagnostic_format {
        DiagnosticFormat::Human => color_stream(),
        DiagnosticFormat::Short => StandardStream::stderr(ColorChoice::Never),
    };

    let mut config = term::Config { tab_width: 2, ..Default::default() };
    if diagnostic_format == DiagnosticFormat::Short {
        config.display_style = term::DisplayStyle::Short;
    }

    for error in errors {
        // The main diagnostic.
        let range = error.range(world);
        let diag = Diagnostic::error()
            .with_message(error.message)
            .with_labels(vec![Label::primary(error.span.source(), range)]);

        term::emit(&mut w, &config, world, &diag)?;

        // Stacktrace-like helper diagnostics.
        for point in error.trace {
            let message = point.v.to_string();
            let help = Diagnostic::help().with_message(message).with_labels(vec![
                Label::primary(
                    point.span.source(),
                    world.source(point.span.source()).range(point.span),
                ),
            ]);

            term::emit(&mut w, &config, world, &help)?;
        }
    }

    Ok(())
}

/// Opens the given file using:
/// - The default file viewer if `open` is `None`.
/// - The given viewer provided by `open` if it is `Some`.
fn open_file(open: Option<&str>, path: &Path) -> StrResult<()> {
    if let Some(app) = open {
        open::with_in_background(path, app);
    } else {
        open::that_in_background(path);
    }

    Ok(())
}

/// Execute a font listing command.
fn fonts(command: FontsSettings) -> StrResult<()> {
    let mut searcher = FontSearcher::new();
    searcher.search(&command.font_paths);

    for (name, infos) in searcher.book.families() {
        println!("{name}");
        if command.variants {
            for info in infos {
                let FontVariant { style, weight, stretch } = info.variant;
                println!("- Style: {style:?}, Weight: {weight:?}, Stretch: {stretch:?}");
            }
        }
    }

    Ok(())
}

/// A world that provides access to the operating system.
struct SystemWorld<'a> {
    root: FileResult<PathBuf>,
    dest: FileResult<PathBuf>,
    library: Prehashed<Library>,
    book: Prehashed<FontBook>,
    fonts: Vec<FontSlot>,
    hashes: RefCell<HashMap<PathBuf, FileResult<PathHash>>>,
    paths: RefCell<HashMap<PathHash, PathSlot>>,
    wpaths: TrackedMut<'a, WriteStorage>,
    sources: FrozenVec<Box<Source>>,
    today: Cell<Option<Datetime>>,
    main: SourceId,
}

/// Holds details about the location of a font and lazily the font itself.
struct FontSlot {
    path: PathBuf,
    index: u32,
    font: OnceCell<Option<Font>>,
}

#[derive(Clone,Debug,Default)]
struct WriteBuffer {
    buffer: RefCell<BTreeMap<u128, Vec<u8>>>, 
}

impl Hash for WriteBuffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let h = self.buffer.borrow();
        for k in h.iter() { //todo! whooops, no order!!
            k.hash(state);
        }
    }
}

impl WriteBuffer {
    fn write(&mut self, at: u128, data: Vec<u8>) -> FileResult<()> {
        let mut a = self.buffer.borrow_mut();
        a.insert(at, data);
        return Ok(());
    }
    fn dump(&self) -> Vec<u8> {
        self.buffer.borrow().values().flat_map(|v| v.clone()).collect()
    }
    fn is_empty(&self) -> bool {
        self.buffer.borrow().is_empty()
    }
}

/// Holds canonical data for all paths pointing to the same entity.
#[derive(Default)]
struct PathSlot {
    source: OnceCell<FileResult<SourceId>>,
    buffer: OnceCell<FileResult<Buffer>>,
}

#[derive(Clone, Debug, Default)]
struct WriteStorage(RefCell<HashMap<PathHash, WriteBuffer>>);

#[comemo::track]
impl WriteStorage {
    fn write(&self, path: PathHash, with: (u128, Vec<u8>)) -> FileResult<()> {
        self.0.borrow_mut().entry(path).or_default().write(with.0, with.1)
    }
    fn dump(&self) -> Vec<(PathHash, WriteBuffer)> {
        self.0.borrow().clone().into_iter().collect()
    }
}



impl<'a> SystemWorld<'a> {
    fn new(
        root: FileResult<PathBuf>,
        dest: FileResult<PathBuf>,
        font_paths: &[PathBuf],
        wp: &'a mut WriteStorage,
    ) -> Self {
        let mut searcher = FontSearcher::new();
        searcher.search(font_paths);

        Self {
            root,
            dest,
            library: Prehashed::new(typst_library::build()),
            book: Prehashed::new(searcher.book),
            fonts: searcher.fonts,
            hashes: RefCell::default(),
            paths: RefCell::default(),
            wpaths: wp.track_mut(),
            sources: FrozenVec::new(),
            today: Cell::new(None),
            main: SourceId::detached(),
        }
    }
}

impl World for SystemWorld<'_> {
    fn root(&self, mode: AccessMode) -> FileResult<&Path> {
        match mode {
            Access::Read(_) => match &self.root {
                Err(e) => Err(e.clone()),
                Ok(p) => Ok(p),
            },
            Access::Write(_) => match &self.dest {
                Err(e) => Err(e.clone()),
                Ok(p) => Ok(p),
            },
        }
    }

    fn library(&self) -> &Prehashed<Library> {
        &self.library
    }

    fn main(&self) -> &Source {
        self.source(self.main)
    }

    #[tracing::instrument(skip_all)]
    fn resolve(&self, path: &Path) -> FileResult<SourceId> {
        self.slot(path)?
            .source
            .get_or_init(|| {
                let path =
                    path.canonicalize().map_err(|f| FileError::from_io(f, path))?;
                let buf = read(&path)?;
                let text = if buf.starts_with(b"\xef\xbb\xbf") {
                    // remove UTF-8 BOM
                    std::str::from_utf8(&buf[3..])?.to_owned()
                } else {
                    // Assume UTF-8
                    String::from_utf8(buf)?
                };
                Ok(self.insert(&path, text))
            })
            .clone()
    }

    fn source(&self, id: SourceId) -> &Source {
        &self.sources[id.as_u16() as usize]
    }

    fn book(&self) -> &Prehashed<FontBook> {
        &self.book
    }

    fn font(&self, id: usize) -> Option<Font> {
        let slot = &self.fonts[id];
        slot.font
            .get_or_init(|| {
                let data = self.read(&slot.path).ok()?;
                Font::new(data, slot.index)
            })
            .clone()
    }

    fn read(&self, path: &Path) -> FileResult<Buffer> {
        self.slot(path)?
            .buffer
            .get_or_init(|| read(path).map(Buffer::from))
            .clone()
    }

    fn write(&self, path: &Path, at: u128, what: Vec<u8>) -> FileResult<()> {
        self.wpaths.write(self.wslot(path)?, (at, what))
    }

    fn today(&self, offset: Option<i64>) -> Option<Datetime> {
        if self.today.get().is_none() {
            let datetime = match offset {
                None => chrono::Local::now().naive_local(),
                Some(o) => (chrono::Utc::now() + chrono::Duration::hours(o)).naive_utc(),
            };

            self.today.set(Some(Datetime::from_ymd(
                datetime.year(),
                datetime.month().try_into().ok()?,
                datetime.day().try_into().ok()?,
            )?))
        }

        self.today.get()
    }
}

impl SystemWorld<'_> {
    #[tracing::instrument(skip_all)]
    fn slot(&self, path: &Path) -> FileResult<RefMut<PathSlot>> {
        let mut hashes = self.hashes.borrow_mut();
        let hash = match hashes.get(path).cloned() {
            Some(hash) => hash,
            None => {
                let hash = PathHash::new(path, AccessMode::R);
                if let Ok(canon) = path.canonicalize() {
                    hashes.insert(canon.normalize(), hash.clone());
                }
                hashes.insert(path.into(), hash.clone());
                hash
            }
        }?;

        Ok(std::cell::RefMut::map(self.paths.borrow_mut(), |paths| {
            paths.entry(hash).or_default()
        }))
    }
    fn wslot(&self, path: &Path) -> FileResult<PathHash> {
        let mut hashes = self.hashes.borrow_mut();
        let hash = match hashes.get(path).cloned() {
            Some(hash) => hash,
            None => {
                let hash = PathHash::new(path, AccessMode::W);
                if let Ok(canon) = path.canonicalize() {
                    hashes.insert(canon.normalize(), hash.clone());
                }
                hashes.insert(path.into(), hash.clone());
                hash
            }
        }?;

        Ok(hash)
    }

    #[tracing::instrument(skip_all)]
    fn insert(&self, path: &Path, text: String) -> SourceId {
        let id = SourceId::from_u16(self.sources.len() as u16);
        let source = Source::new(id, path, text);
        self.sources.push(Box::new(source));
        id
    }

    fn relevant(&mut self, event: &notify::Event) -> bool {
        match &event.kind {
            notify::EventKind::Any => {}
            notify::EventKind::Access(_) => return false,
            notify::EventKind::Create(_) => return true,
            notify::EventKind::Modify(kind) => match kind {
                notify::event::ModifyKind::Any => {}
                notify::event::ModifyKind::Data(_) => {}
                notify::event::ModifyKind::Metadata(_) => return false,
                notify::event::ModifyKind::Name(_) => return true,
                notify::event::ModifyKind::Other => return false,
            },
            notify::EventKind::Remove(_) => {}
            notify::EventKind::Other => return false,
        }

        event.paths.iter().any(|path| self.dependant(path))
    }

    fn dependant(&self, path: &Path) -> bool {
        self.hashes.borrow().contains_key(&path.normalize())
            || PathHash::new(path, AccessMode::R)
                .map_or(false, |hash| self.paths.borrow().contains_key(&hash))
    }

    #[tracing::instrument(skip_all)]
    fn reset(&mut self) {
        self.sources.as_mut().clear();
        self.hashes.borrow_mut().clear();
        self.paths.borrow_mut().clear();
        self.today.set(None);
    }
}

/// A hash that is the same for all paths pointing to the same entity.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct PathHash(u128);

impl PathHash {
    fn new(path: &Path, mode: AccessMode) -> FileResult<Self> {
        let f = |e| FileError::from_io(e, path);
        let handle = match mode {
            Access::Read(_) => Handle::from_path(path).map_err(f)?, //note: opening twice???
            Access::Write(_) => {
                //Path has been validated, so we can create all misssing directories
                fs::create_dir_all(path.parent().ok_or(FileError::AccessDenied)?)
                    .map_err(f)?;
                let file = File::create(path).map_err(f)?;
                Handle::from_file(file).map_err(f)?
            }
        };
        let state = hash128(&handle);
        Ok(Self(state))
    }
}

/// Read a file.
#[tracing::instrument(skip_all)]
fn read(path: &Path) -> FileResult<Vec<u8>> {
    let f = |e| FileError::from_io(e, path);
    if fs::metadata(path).map_err(f)?.is_dir() {
        Err(FileError::IsDirectory)
    } else {
        fs::read(path).map_err(f)
    }
}

impl<'a> codespan_reporting::files::Files<'a> for SystemWorld<'_> {
    type FileId = SourceId;
    type Name = std::path::Display<'a>;
    type Source = &'a str;

    fn name(&'a self, id: SourceId) -> CodespanResult<Self::Name> {
        Ok(World::source(self, id).path().display())
    }

    fn source(&'a self, id: SourceId) -> CodespanResult<Self::Source> {
        Ok(World::source(self, id).text())
    }

    fn line_index(&'a self, id: SourceId, given: usize) -> CodespanResult<usize> {
        let source = World::source(self, id);
        source
            .byte_to_line(given)
            .ok_or_else(|| CodespanError::IndexTooLarge {
                given,
                max: source.len_bytes(),
            })
    }

    fn line_range(
        &'a self,
        id: SourceId,
        given: usize,
    ) -> CodespanResult<std::ops::Range<usize>> {
        let source = World::source(self, id);
        source
            .line_to_range(given)
            .ok_or_else(|| CodespanError::LineTooLarge { given, max: source.len_lines() })
    }

    fn column_number(
        &'a self,
        id: SourceId,
        _: usize,
        given: usize,
    ) -> CodespanResult<usize> {
        let source = World::source(self, id);
        source.byte_to_column(given).ok_or_else(|| {
            let max = source.len_bytes();
            if given <= max {
                CodespanError::InvalidCharBoundary { given }
            } else {
                CodespanError::IndexTooLarge { given, max }
            }
        })
    }
}

/// Searches for fonts.
struct FontSearcher {
    book: FontBook,
    fonts: Vec<FontSlot>,
}

impl FontSearcher {
    /// Create a new, empty system searcher.
    fn new() -> Self {
        Self { book: FontBook::new(), fonts: vec![] }
    }

    /// Search everything that is available.
    fn search(&mut self, font_paths: &[PathBuf]) {
        self.search_system();

        #[cfg(feature = "embed-fonts")]
        self.search_embedded();

        for path in font_paths {
            self.search_dir(path)
        }
    }

    /// Add fonts that are embedded in the binary.
    #[cfg(feature = "embed-fonts")]
    fn search_embedded(&mut self) {
        let mut search = |bytes: &'static [u8]| {
            let buffer = Buffer::from_static(bytes);
            for (i, font) in Font::iter(buffer).enumerate() {
                self.book.push(font.info().clone());
                self.fonts.push(FontSlot {
                    path: PathBuf::new(),
                    index: i as u32,
                    font: OnceCell::from(Some(font)),
                });
            }
        };

        // Embed default fonts.
        search(include_bytes!("../../assets/fonts/LinLibertine_R.ttf"));
        search(include_bytes!("../../assets/fonts/LinLibertine_RB.ttf"));
        search(include_bytes!("../../assets/fonts/LinLibertine_RBI.ttf"));
        search(include_bytes!("../../assets/fonts/LinLibertine_RI.ttf"));
        search(include_bytes!("../../assets/fonts/NewCMMath-Book.otf"));
        search(include_bytes!("../../assets/fonts/NewCMMath-Regular.otf"));
        search(include_bytes!("../../assets/fonts/NewCM10-Regular.otf"));
        search(include_bytes!("../../assets/fonts/NewCM10-Bold.otf"));
        search(include_bytes!("../../assets/fonts/NewCM10-Italic.otf"));
        search(include_bytes!("../../assets/fonts/NewCM10-BoldItalic.otf"));
        search(include_bytes!("../../assets/fonts/DejaVuSansMono.ttf"));
        search(include_bytes!("../../assets/fonts/DejaVuSansMono-Bold.ttf"));
        search(include_bytes!("../../assets/fonts/DejaVuSansMono-Oblique.ttf"));
        search(include_bytes!("../../assets/fonts/DejaVuSansMono-BoldOblique.ttf"));
    }

    /// Search for fonts in the linux system font directories.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn search_system(&mut self) {
        self.search_dir("/usr/share/fonts");
        self.search_dir("/usr/local/share/fonts");

        if let Some(dir) = dirs::font_dir() {
            self.search_dir(dir);
        }
    }

    /// Search for fonts in the macOS system font directories.
    #[cfg(target_os = "macos")]
    fn search_system(&mut self) {
        self.search_dir("/Library/Fonts");
        self.search_dir("/Network/Library/Fonts");
        self.search_dir("/System/Library/Fonts");

        if let Some(dir) = dirs::font_dir() {
            self.search_dir(dir);
        }
    }

    /// Search for fonts in the Windows system font directories.
    #[cfg(windows)]
    fn search_system(&mut self) {
        let windir =
            std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());

        self.search_dir(Path::new(&windir).join("Fonts"));

        if let Some(roaming) = dirs::config_dir() {
            self.search_dir(roaming.join("Microsoft\\Windows\\Fonts"));
        }

        if let Some(local) = dirs::cache_dir() {
            self.search_dir(local.join("Microsoft\\Windows\\Fonts"));
        }
    }

    /// Search for all fonts in a directory recursively.
    fn search_dir(&mut self, path: impl AsRef<Path>) {
        for entry in WalkDir::new(path)
            .follow_links(true)
            .sort_by(|a, b| a.file_name().cmp(b.file_name()))
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("ttf" | "otf" | "TTF" | "OTF" | "ttc" | "otc" | "TTC" | "OTC"),
            ) {
                self.search_file(path);
            }
        }
    }

    /// Index the fonts in the file at the given path.
    fn search_file(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if let Ok(file) = File::open(path) {
            if let Ok(mmap) = unsafe { Mmap::map(&file) } {
                for (i, info) in FontInfo::iter(&mmap).enumerate() {
                    self.book.push(info);
                    self.fonts.push(FontSlot {
                        path: path.into(),
                        index: i as u32,
                        font: OnceCell::new(),
                    });
                }
            }
        }
    }
}
