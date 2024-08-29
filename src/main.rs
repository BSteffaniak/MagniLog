#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use std::str;
use std::sync::{Arc, LazyLock};
use std::{path::Path, pin::Pin};

use bytesize::ByteSize;
use clap::Parser;
use free_log_client::LogsInitError;
use kanal::{AsyncReceiver, AsyncSender, SendError};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, BufReader};
use tokio::sync::RwLock;
use tokio::task::{JoinError, JoinHandle};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    file: String,
    #[arg(short, long)]
    threads: Option<usize>,
}

#[derive(Debug, Error)]
pub enum MagniLogError {
    #[error(transparent)]
    ReadLog(#[from] ReadLogError),
    #[error(transparent)]
    LogsInit(#[from] LogsInitError),
}

fn main() -> Result<(), MagniLogError> {
    let args = Args::parse();
    let threads = args.threads.unwrap_or(8);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(threads + 1)
        .build()
        .unwrap();

    runtime.block_on(async move {
        let logs_config = free_log_client::LogsConfig::builder();

        #[cfg(debug_assertions)]
        const DEFAULT_LOG_LEVEL: &str = "magnilog=trace";
        #[cfg(not(debug_assertions))]
        const DEFAULT_LOG_LEVEL: &str = "magnilog=info";

        free_log_client::init(logs_config.env_filter(match option_env!("MAGNILOG_LOG") {
            Some(v) => v,
            None => match option_env!("RUST_LOG") {
                Some(v) => v,
                None => DEFAULT_LOG_LEVEL,
            },
        }))?;

        read_log(&args.file, threads).await?;

        Ok(())
    })
}

#[derive(Debug, Clone)]
pub struct Message {
    pub message: String,
    pub level: String,
    pub ts: usize,
    pub target: String,
    pub module_path: String,
    pub location: String,
}

#[derive(Debug, Clone)]
pub struct MessageLocation {
    pub level: String,
    pub ts: usize,
    pub start: usize,
    pub end: usize,
}

pub struct LogCollection {
    receiver: AsyncReceiver<Option<MessageLocation>>,
    messages: Vec<MessageLocation>,
}

impl LogCollection {
    pub fn new(receiver: AsyncReceiver<Option<MessageLocation>>) -> Self {
        Self {
            receiver,
            messages: vec![],
        }
    }

    pub async fn listen(&mut self) {
        while let Ok(Some(message)) = self.receiver.recv().await {
            self.messages.push(message);
        }
    }
}

static LOG_POSITION_START_REGEX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"(^|[\n\r])\{"level":"(\w+)","ts":(\d+)"#).unwrap());
static LOG_START_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(^|[\n\r])\{"level":"(\w+)","ts":(\d+),"values":\[(("(?:[^"\\]|\\.)*"(\s*,\s*)?)*)\],"target":"((?:[^"\\]|\\.)*)","modulePath":"((?:[^"\\]|\\.)*)","location":"((?:[^"\\]|\\.)*)""#).unwrap()
});

#[allow(unused)]
fn parse_message(log: &str) -> Message {
    let matches = LOG_START_REGEX.captures_iter(log);

    let caps = matches.into_iter().next().unwrap();

    for i in 0..caps.len() {
        log::trace!("parse_message: match {i}: {:?}\n", caps.get(i));
    }
    let level = if let Some(level) = caps.get(2) {
        let level = level.as_str();
        level.to_string()
    } else {
        "".to_string()
    };
    let ts = if let Some(ts) = caps.get(3) {
        let ts = ts.as_str();
        ts.to_string()
    } else {
        "".to_string()
    };
    let target = if let Some(target) = caps.get(7) {
        let target = target.as_str();
        target.to_string()
    } else {
        "".to_string()
    };
    let module_path = if let Some(module_path) = caps.get(8) {
        let module_path = module_path.as_str();
        module_path.to_string()
    } else {
        "".to_string()
    };
    let location = if let Some(location) = caps.get(9) {
        let location = location.as_str();
        location.to_string()
    } else {
        "".to_string()
    };

    let value = caps.get(0).unwrap();

    Message {
        message: "".into(),
        level,
        ts: ts.parse::<usize>().unwrap_or_else(|e| {
            log::warn!("Failed to parse timestamp: {ts} ({e:?})");
            0
        }),
        target,
        module_path,
        location,
    }
}

#[derive(Debug, Error)]
pub enum ReadLogError {
    #[error(transparent)]
    IO(#[from] std::io::Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Send(#[from] SendError),
    #[error(transparent)]
    Join(#[from] JoinError),
}

pub trait AsyncReadSeek: AsyncRead + AsyncSeek + Send + Sync {}

impl<R: AsyncRead + AsyncSeek + Send + Sync> AsyncReadSeek for BufReader<R> {}
impl AsyncReadSeek for File {}

const BUFFER_SIZE: usize = 1024 * 32;

pub struct LogReader {
    sender: AsyncSender<Option<MessageLocation>>,
    offset: usize,
    len: usize,
    reader: Pin<Box<dyn AsyncReadSeek>>,
    buffer: [u8; BUFFER_SIZE],
    #[allow(unused)]
    current_message: Arc<RwLock<Option<String>>>,
}

impl LogReader {
    fn new(
        sender: AsyncSender<Option<MessageLocation>>,
        reader: impl AsyncReadSeek + 'static,
        offset: usize,
        len: usize,
    ) -> Self {
        Self {
            sender,
            offset,
            len,
            reader: Box::pin(reader),
            buffer: [0; 1024 * 32],
            current_message: Arc::new(RwLock::new(None)),
        }
    }

    async fn read(&mut self) -> Result<(), ReadLogError> {
        self.reader
            .seek(std::io::SeekFrom::Start(self.offset as u64))
            .await?;

        let mut read = 0;

        let mut current_message = "".to_owned();

        while read < self.len {
            let size = if read + BUFFER_SIZE >= self.len {
                self.reader
                    .read_exact(&mut self.buffer[..self.len % BUFFER_SIZE])
                    .await?
            } else {
                self.reader.read(&mut self.buffer).await?
            };

            if size == 0 {
                break;
            }

            let logs = str::from_utf8(&self.buffer[..size])?;
            current_message.push_str(logs);

            let end = self
                .parse_logs(read + self.offset, &current_message)
                .await?;

            // handle unicode??
            current_message = current_message.as_str()[end..].to_owned();

            read += size;
        }

        Ok(())
    }

    async fn parse_logs(&self, offset: usize, logs: &str) -> Result<usize, SendError> {
        let matches = LOG_POSITION_START_REGEX.captures_iter(logs);

        let mut end = 0;

        for caps in matches {
            for i in 0..caps.len() {
                log::trace!("parse_logs: match {i}: {:?}\n", caps.get(i));
            }
            let level = if let Some(level) = caps.get(2) {
                let level = level.as_str();
                level.to_string()
            } else {
                "".to_string()
            };
            let ts = if let Some(ts) = caps.get(3) {
                let ts = ts.as_str();
                ts.to_string()
            } else {
                "".to_string()
            };

            let value = caps.get(0).unwrap();

            end = value.end();

            let message = MessageLocation {
                level,
                ts: ts.parse::<usize>().unwrap_or_else(|e| {
                    log::warn!("Failed to parse timestamp: {ts} ({e:?})");
                    0
                }),
                start: offset + value.start(),
                end: offset + value.end(),
            };

            self.sender.send(Some(message)).await?;
        }

        Ok(end)
    }
}

async fn read_log<P: AsRef<Path>>(log: P, threads: usize) -> Result<(), ReadLogError> {
    let start = std::time::SystemTime::now();

    let file = tokio::fs::OpenOptions::new()
        .create(false)
        .write(false)
        .read(true)
        .open(&log)
        .await?;

    let metadata = file.metadata().await?;
    let len = metadata.len() as usize;

    let (tx, rx) = kanal::unbounded_async();

    let collection = Arc::new(RwLock::new(LogCollection::new(rx)));

    tokio::task::spawn({
        let collection = collection.clone();
        async move {
            let mut collection = collection.write().await;
            log::debug!("Listening for messages");
            collection.listen().await
        }
    });

    let sections = threads;
    let chunk_size = len / sections;
    let mut workers = vec![];

    for section in 0..sections {
        let offset = section * chunk_size;
        let len = if section == sections - 1 {
            len - chunk_size * (sections - 1)
        } else {
            chunk_size
        };

        workers.push(read_log_section(&log, tx.clone(), offset, len).await?);
    }

    let responses = futures::future::join_all(workers).await;
    tx.send(None).await?;

    let errors = responses
        .into_iter()
        .flatten()
        .filter_map(|x| x.err())
        .collect::<Vec<_>>();

    if !errors.is_empty() {
        log::error!("Errors: {errors:?}");
        return Err(errors.into_iter().next().unwrap());
    }

    let end = std::time::SystemTime::now();
    log::info!(
        "Finished read in {}ms {}",
        end.duration_since(start).unwrap().as_millis(),
        ByteSize(len as u64),
    );

    let collection = collection.read().await;
    let messages = &collection.messages;

    log::debug!("{} total messages", messages.len());

    Ok(())
}

async fn read_log_section<P: AsRef<Path>>(
    log: P,
    sender: AsyncSender<Option<MessageLocation>>,
    offset: usize,
    len: usize,
) -> Result<JoinHandle<Result<(), ReadLogError>>, ReadLogError> {
    log::debug!("read_log_section: offset={offset} len={len}");
    let file = tokio::fs::OpenOptions::new()
        .create(false)
        .write(false)
        .read(true)
        .open(&log)
        .await?;

    let reader = BufReader::new(file);
    let mut reader = LogReader::new(sender.clone(), reader, offset, len);

    let read_finished = tokio::task::spawn(async move { reader.read().await });

    Ok(read_finished)
}
