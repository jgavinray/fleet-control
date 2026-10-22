// Options shared by the CLI and the engine checks (Python: argparse namespace in main()).
#[derive(Debug, Clone)]
pub struct Opts {
    pub quick: bool,
    pub json: bool,
    pub registry: Option<std::path::PathBuf>,
    pub timeout: f64,
    pub bg_timeout: f64,
    pub force_shared: bool,
    pub engines: Vec<String>,
}
