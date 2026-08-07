from pathlib import Path

path = Path("src/http3.rs")
text = path.read_text()
old_start = '''pub fn start(runtime: Arc<RuntimeConfig>) -> Result<()> {\n    let server = &runtime.config.server;\n    let allow_early_data = server.http3_enable_early_data;\n'''
new_start = '''pub fn start(runtime: Arc<RuntimeConfig>) -> Result<()> {\n    let server = &runtime.config.server;\n'''
if old_start not in text:
    raise SystemExit("mis-scoped start early-data variable not found")
text = text.replace(old_start, new_start, 1)
old_run = '''async fn run(\n    runtime: Arc<RuntimeConfig>,\n    ready: mpsc::SyncSender<Result<(), String>>,\n) -> Result<()> {\n    let server = &runtime.config.server;\n'''
new_run = '''async fn run(\n    runtime: Arc<RuntimeConfig>,\n    ready: mpsc::SyncSender<Result<(), String>>,\n) -> Result<()> {\n    let server = &runtime.config.server;\n    let allow_early_data = server.http3_enable_early_data;\n'''
if old_run not in text:
    raise SystemExit("run function insertion point not found")
path.write_text(text.replace(old_run, new_run, 1))
