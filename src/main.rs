#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use std::str;
use std::{path::Path, pin::Pin};

use clap::Parser;
use thiserror::Error;
use tokio::io::{AsyncReadExt, BufReader};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    file: String,
}

#[derive(Debug, Error)]
pub enum MagniLogError {
    #[error(transparent)]
    ReadLog(#[from] ReadLogError),
}

#[tokio::main]
async fn main() -> Result<(), MagniLogError> {
    let args = Args::parse();

    read_log(&args.file).await?;

    Ok(())
}

pub struct LogReader {
    reader: Pin<Box<dyn tokio::io::AsyncRead>>,
    buffer: [u8; 1024 * 32],
}

impl LogReader {
    fn new(reader: impl tokio::io::AsyncRead + 'static) -> Self {
        Self {
            reader: Box::pin(reader),
            buffer: [0; 1024 * 32],
        }
    }

    async fn read(&mut self) -> Result<(), ReadLogError> {
        while let Ok(size) = self.reader.read(&mut self.buffer).await {
            if size == 0 {
                break;
            };
            parse_logs(str::from_utf8(&self.buffer[..size])?)
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ReadLogError {
    #[error(transparent)]
    IO(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
}

fn parse_logs(logs: &str) {
    println!("{}", logs)
}

async fn read_log<P: AsRef<Path>>(log: P) -> Result<(), ReadLogError> {
    let file = tokio::fs::OpenOptions::new()
        .create(false)
        .write(false)
        .read(true)
        .open(log)
        .await?;

    let reader = BufReader::new(file);

    let mut log_reader = LogReader::new(reader);
    log_reader.read().await?;

    Ok(())
}
