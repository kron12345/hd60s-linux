use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use hd60s_linux::trace::analyze_tsv;

fn run() -> Result<(), String> {
    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments.len() != 3 {
        return Err(format!(
            "usage: {} INPUT.tsv OUTPUT.tsv",
            arguments
                .first()
                .map(String::as_str)
                .unwrap_or("analyze-usb-trace")
        ));
    }

    let input_path = Path::new(&arguments[1]);
    let output_path = Path::new(&arguments[2]);
    let input = fs::read_to_string(input_path)
        .map_err(|error| format!("reading {}: {error}", input_path.display()))?;
    let analysis = analyze_tsv(&input)?;

    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_path)
        .map_err(|error| format!("creating {}: {error}", output_path.display()))?;
    output
        .write_all(analysis.sanitized_tsv.as_bytes())
        .map_err(|error| format!("writing {}: {error}", output_path.display()))?;

    print!("{}", analysis.summary.render());
    eprintln!("Sanitized trace written to {}", output_path.display());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
