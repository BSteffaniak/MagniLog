#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use std::str;
use std::sync::{Arc, LazyLock};
use std::{path::Path, pin::Pin};

use bytesize::ByteSize;
use clap::Parser;
use free_log_client::LogsInitError;
use kanal::{AsyncReceiver, AsyncSender, SendError};
use serde_json::Value;
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
    #[arg(short, long)]
    offset: Option<usize>,
    #[arg(short, long)]
    limit: Option<usize>,
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

        read_log(&args.file, args.offset, args.limit, threads).await?;

        Ok(())
    })
}

#[derive(Debug, Clone)]
pub struct Message {
    pub message_location: MessageLocation,
    pub messages: Vec<String>,
    pub target: String,
    pub module_path: String,
    pub location: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MessageLocation {
    pub level: String,
    pub ts: String,
    pub start: usize,
    pub end: usize,
}

pub struct TempMessageLocation {
    pub level: String,
    pub ts: String,
    pub start: usize,
}

const MESSAGE_BUFFER_SIZE: usize = 1024 * 32;
static MESSAGE_BUFFER: LazyLock<RwLock<[u8; MESSAGE_BUFFER_SIZE]>> =
    LazyLock::new(|| RwLock::new([0; MESSAGE_BUFFER_SIZE]));

impl MessageLocation {
    pub async fn load(
        self,
        reader: &mut (impl AsyncReadSeek + Unpin),
    ) -> Result<Message, ReadLogError> {
        let size = self.end - self.start;
        log::debug!("load: start={} end={} size={}", self.start, self.end, size);

        futures::pin_mut!(reader);
        reader
            .seek(std::io::SeekFrom::Start(self.start as u64))
            .await?;

        let mut buffer = MESSAGE_BUFFER.write().await;

        let data = reader.read_exact(&mut buffer[..size]).await?;
        let log = str::from_utf8(&buffer[..data])?;

        parse_message(log, self)
    }
}

impl PartialOrd for MessageLocation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.ts.cmp(&other.ts))
    }
}

impl Ord for MessageLocation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ts.cmp(&other.ts)
    }
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
    LazyLock::new(|| regex::Regex::new(r#"(^|[\n\r])\{"level":"(\w+)","ts":([^,]+),"#).unwrap());
static LOG_START_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(^|[\n\r])\{"level":"(\w+)","ts":([^,]+),"values":(\[("(?:[^"\\]|\\.)*"(\s*,\s*)?)*\]),"target":"((?:[^"\\]|\\.)*)","modulePath":"((?:[^"\\]|\\.)*)","location":"((?:[^"\\]|\\.)*)""#).unwrap()
});

fn parse_message(log: &str, message_location: MessageLocation) -> Result<Message, ReadLogError> {
    let matches = LOG_START_REGEX.captures_iter(log);

    log::debug!("parsing message: '{log}'");
    let Some(caps) = matches.into_iter().next() else {
        panic!("Failed to parse message '{log}'\nmessage_location={message_location:?}");
    };

    for i in 0..caps.len() {
        log::trace!("parse_message: match {i}: {:?}\n", caps.get(i));
    }
    let messages = if let Some(messages) = caps.get(4) {
        let messages = messages.as_str();
        let messages: Value = serde_json::from_str(messages)?;
        messages
            .as_array()
            .map(|x| {
                x.iter()
                    .filter_map(|x| x.as_str().map(|x| x.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    } else {
        vec![]
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

    Ok(Message {
        message_location,
        messages,
        target,
        module_path,
        location,
    })
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
    Serde(#[from] serde_json::Error),
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
    start_message: Arc<RwLock<String>>,
    end_message: Arc<RwLock<String>>,
    temp_message_location: Option<TempMessageLocation>,
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
            start_message: Arc::new(RwLock::new("".to_owned())),
            end_message: Arc::new(RwLock::new("".to_owned())),
            temp_message_location: None,
        }
    }

    async fn read(&mut self) -> Result<(), ReadLogError> {
        self.reader
            .seek(std::io::SeekFrom::Start(self.offset as u64))
            .await?;

        let mut read = 0;
        let mut last_start = None;

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

            if let Some((first_start, start, _end)) = self
                .parse_logs(read + self.offset, &current_message)
                .await?
            {
                if read == 0 {
                    *self.start_message.write().await =
                        current_message.as_str()[..first_start].to_owned();
                }

                // handle unicode??
                if read + size < self.len {
                    current_message = "".to_owned();
                } else {
                    last_start.replace(start);
                }
            }

            read += size;
        }

        if let Some(start) = last_start {
            *self.end_message.write().await = current_message.as_str()[start..].to_owned();
        }

        Ok(())
    }

    async fn parse_logs(
        &mut self,
        offset: usize,
        logs: &str,
    ) -> Result<Option<(usize, usize, usize)>, SendError> {
        let matches = LOG_POSITION_START_REGEX.captures_iter(logs);

        let mut first_start = None;
        let mut start = None;
        let mut end = None;

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

            if first_start.is_none() {
                first_start = Some(value.start());
            }
            start = Some(value.start());
            end = Some(value.end());

            if let Some(temp) = self.temp_message_location.take() {
                let message = MessageLocation {
                    level: temp.level,
                    ts: temp.ts,
                    start: temp.start,
                    end: offset + value.start(),
                };
                self.sender.send(Some(message)).await?;
            }

            self.temp_message_location.replace(TempMessageLocation {
                level,
                ts,
                start: offset + value.start(),
            });
        }

        if let (Some(first_start), Some(start), Some(end)) = (first_start, start, end) {
            Ok(Some((first_start, start, end)))
        } else {
            Ok(None)
        }
    }
}

async fn read_log<P: AsRef<Path>>(
    log: P,
    offset: Option<usize>,
    limit: Option<usize>,
    threads: usize,
) -> Result<(), ReadLogError> {
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
    let mut readers = vec![];
    let mut workers = vec![];

    for section in 0..sections {
        let offset = section * chunk_size;
        let len = if section == sections - 1 {
            len - chunk_size * (sections - 1)
        } else {
            chunk_size
        };

        let (reader, worker) = read_log_section(&log, tx.clone(), offset, len).await?;

        readers.push(reader);
        workers.push(worker);
    }

    let responses = futures::future::join_all(workers).await;

    // stitch together individual readers and get the messages that were in the gap
    for (prev, reader) in readers.iter().zip(readers.iter().skip(1)) {
        let mut prev = prev.write().await;
        let end_message = prev.end_message.read().await.to_owned();
        let reader = reader.read().await;
        let start_message = reader.start_message.read().await;

        if let Some(temp) = prev.temp_message_location.take() {
            let message = MessageLocation {
                level: temp.level,
                ts: temp.ts,
                start: temp.start,
                end: temp.start + end_message.len() + start_message.len(),
            };
            prev.sender.send(Some(message)).await?;
        }
    }

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

    let mut collection = collection.write().await;
    let start = std::time::SystemTime::now();
    log::debug!("Sorting messages");
    collection.messages.sort();
    let end = std::time::SystemTime::now();
    log::debug!(
        "Sorted messages in {}ms",
        end.duration_since(start).unwrap().as_millis(),
    );
    let messages = &collection.messages;

    let file = tokio::fs::OpenOptions::new()
        .create(false)
        .write(false)
        .read(true)
        .open(&log)
        .await?;

    let mut reader = BufReader::new(file);

    if let Some(offset) = offset {
        let limit = limit.unwrap_or(messages.len());
        for message in messages.iter().skip(offset).take(limit) {
            let message = message.to_owned().load(&mut reader).await?;
            log::debug!("Message: {message:?}");
            println!("{}", message.messages.join("\n"));
        }
    }

    log::debug!("{} total messages", messages.len());

    Ok(())
}

async fn read_log_section<P: AsRef<Path>>(
    log: P,
    sender: AsyncSender<Option<MessageLocation>>,
    offset: usize,
    len: usize,
) -> Result<(Arc<RwLock<LogReader>>, JoinHandle<Result<(), ReadLogError>>), ReadLogError> {
    log::debug!("read_log_section: offset={offset} len={len}");
    let file = tokio::fs::OpenOptions::new()
        .create(false)
        .write(false)
        .read(true)
        .open(&log)
        .await?;

    let reader = BufReader::new(file);
    let reader = Arc::new(RwLock::new(LogReader::new(
        sender.clone(),
        reader,
        offset,
        len,
    )));

    let read_finished = tokio::task::spawn({
        let reader = reader.clone();
        async move {
            let mut reader = reader.write().await;
            reader.read().await
        }
    });

    Ok((reader, read_finished))
}
