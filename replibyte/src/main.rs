#[macro_use]
extern crate prettytable;

use std::fs::File;
use std::io::{stdin, BufReader, Error, ErrorKind, Read};
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use timeago::Formatter;

use utils::to_human_readable_unit;

use crate::bridge::s3::S3;
use crate::bridge::{Bridge, ReadOptions};
use crate::cli::{BackupCommand, SubCommand, TransformerCommand, CLI};
use crate::config::{Config, ConnectionUri};
use crate::connector::Connector;
use crate::destination::mongodb::MongoDB as DestinationMongoDB;
use crate::destination::mongodb_stdout::MongoDBStdout;
use crate::destination::postgres::Postgres as DestinationPostgres;
use crate::destination::postgres_stdout::PostgresStdout;
use crate::source::mongodb::MongoDB as SourceMongoDB;
use crate::source::mongodb_stdin::MongoDBStdin;
use crate::source::postgres::Postgres as SourcePostgres;
use crate::source::postgres_stdin::PostgresStdin;
use crate::source::{Source, SourceOptions};
use crate::tasks::full_backup::FullBackupTask;
use crate::tasks::full_restore::FullRestoreTask;
use crate::tasks::{MaxBytes, Task, TransferredBytes};
use crate::transformer::transformers;
use crate::utils::{epoch_millis, table};

mod bridge;
mod cli;
mod config;
mod connector;
mod destination;
mod runtime;
mod source;
mod tasks;
mod transformer;
mod types;
mod utils;

fn list_backups(s3: &mut S3) -> Result<(), Error> {
    let _ = s3.init()?;
    let mut index_file = s3.index_file()?;

    if index_file.backups.is_empty() {
        println!("<empty> no backups available\n");
        return Ok(());
    }

    index_file.backups.sort_by(|a, b| a.cmp(b).reverse());

    let mut table = table();
    table.set_titles(row!["name", "size", "when", "compressed", "encrypted"]);
    let formatter = Formatter::new();
    let now = epoch_millis();

    for backup in index_file.backups {
        table.add_row(row![
            backup.directory_name.as_str(),
            to_human_readable_unit(backup.size),
            formatter.convert(Duration::from_millis((now - backup.created_at) as u64)),
            backup.compressed,
            backup.encrypted,
        ]);
    }

    let _ = table.printstd();

    Ok(())
}

fn show_progress_bar(rx_pb: Receiver<(TransferredBytes, MaxBytes)>) {
    let pb = ProgressBar::new(0);
    pb.set_style(ProgressStyle::default_spinner());

    let mut style_is_progress_bar = false;
    let mut _max_bytes = 0usize;
    let mut last_transferred_bytes = 0usize;

    loop {
        let (transferred_bytes, max_bytes) = match rx_pb.try_recv() {
            Ok(msg) => msg,
            Err(_) => (last_transferred_bytes, _max_bytes),
        };

        if _max_bytes == 0 && style_is_progress_bar {
            // show spinner if there is no max_bytes indicated
            pb.set_style(ProgressStyle::default_spinner());
            style_is_progress_bar = false;
        } else if _max_bytes > 0 && !style_is_progress_bar {
            pb.set_style(ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.green/blue}] {bytes}/{total_bytes} ({eta})")
                .progress_chars("#>-"));
            style_is_progress_bar = true;
        }

        if max_bytes != _max_bytes {
            pb.set_length(max_bytes as u64);
            _max_bytes = max_bytes;
        }

        last_transferred_bytes = transferred_bytes;
        pb.set_position(transferred_bytes as u64);

        sleep(Duration::from_micros(50));
    }
}

/// display all transformers available
fn list_transformers() {
    let mut table = table();
    table.set_titles(row!["name", "description"]);

    for transformer in transformers() {
        table.add_row(row![transformer.id(), transformer.description()]);
    }

    let _ = table.printstd();
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = CLI::parse();

    let file = File::open(args.config)?;
    let config: Config = serde_yaml::from_reader(file)?;

    let mut bridge = S3::new(
        config.bridge.bucket()?,
        config.bridge.region()?,
        config.bridge.access_key_id()?,
        config.bridge.secret_access_key()?,
        config.bridge.endpoint()?,
    );

    match &config.source {
        Some(source) => {
            bridge.set_compression(source.compression.unwrap_or(true));
            bridge.set_encryption_key(source.encryption_key()?)
        }
        None => {}
    }

    match &config.destination {
        Some(dest) => {
            bridge.set_compression(dest.compression.unwrap_or(true));
            bridge.set_encryption_key(dest.encryption_key()?);
        }
        None => {}
    }

    let (tx_pb, rx_pb) = mpsc::sync_channel::<(TransferredBytes, MaxBytes)>(1000);

    let sub_commands: &SubCommand = &args.sub_commands;

    match sub_commands {
        // skip progress when output = true
        SubCommand::Restore(args) if args.output => {}
        _ => {
            let _ = thread::spawn(move || show_progress_bar(rx_pb));
        }
    };

    let progress_callback = |bytes: TransferredBytes, max_bytes: MaxBytes| {
        let _ = tx_pb.send((bytes, max_bytes));
    };

    match sub_commands {
        SubCommand::Backup(cmd) => match cmd {
            BackupCommand::List => {
                let _ = list_backups(&mut bridge)?;
            }
            BackupCommand::Run(args) => match config.source {
                Some(source) => {
                    // Match the transformers from the config
                    let transformers = source
                        .transformers
                        .iter()
                        .flat_map(|transformer| {
                            transformer.columns.iter().map(|column| {
                                column.transformer.transformer(
                                    transformer.database.as_str(),
                                    transformer.table.as_str(),
                                    column.name.as_str(),
                                )
                            })
                        })
                        .collect::<Vec<_>>();

                    let empty_config = vec![];
                    let skip_config = match &source.skip {
                        Some(config) => config,
                        None => &empty_config,
                    };

                    let options = SourceOptions {
                        transformers: &transformers,
                        skip_config: &skip_config,
                    };

                    match args.source_type.as_ref().map(|x| x.as_str()) {
                        None => match source.connection_uri()? {
                            ConnectionUri::Postgres(host, port, username, password, database) => {
                                let postgres = SourcePostgres::new(
                                    host.as_str(),
                                    port,
                                    database.as_str(),
                                    username.as_str(),
                                    password.as_str(),
                                );

                                let task = FullBackupTask::new(postgres, bridge, options);
                                task.run(progress_callback)?
                            }
                            ConnectionUri::Mysql(host, port, username, password, database) => {
                                todo!() // FIXME
                            }
                            ConnectionUri::MongoDB(
                                host,
                                port,
                                username,
                                password,
                                database,
                                authentication_db,
                            ) => {
                                let mongodb = SourceMongoDB::new(
                                    host.as_str(),
                                    port,
                                    database.as_str(),
                                    username.as_str(),
                                    password.as_str(),
                                    authentication_db.as_str(),
                                );

                                let task = FullBackupTask::new(mongodb, bridge, options);
                                task.run(progress_callback)?
                            }
                        },
                        // some user use "postgres" and "postgresql" both are valid
                        Some(v) if v == "postgres" || v == "postgresql" => {
                            if args.file.is_some() {
                                let dump_file = File::open(args.file.as_ref().unwrap())?;
                                let mut stdin = stdin(); // FIXME
                                let reader = BufReader::new(dump_file);
                                let _ = stdin.read_to_end(&mut reader.buffer().to_vec())?;
                            }

                            let postgres = PostgresStdin::default();
                            let task = FullBackupTask::new(postgres, bridge, options);
                            task.run(progress_callback)?
                        }
                        Some(v) => {
                            return Err(anyhow::Error::from(Error::new(
                                ErrorKind::Other,
                                format!("source type '{}' not recognized", v),
                            )));
                        }
                    }

                    println!("Backup successful!")
                }
                None => {
                    return Err(anyhow::Error::from(Error::new(
                        ErrorKind::Other,
                        "missing <source> object in the configuration file",
                    )));
                }
            },
        },
        SubCommand::Transformer(cmd) => match cmd {
            TransformerCommand::List => {
                let _ = list_transformers();
            }
        },
        SubCommand::Restore(cmd) => match config.destination {
            Some(destination) => {
                let options = match cmd.value.as_str() {
                    "latest" => ReadOptions::Latest,
                    v => ReadOptions::Backup {
                        name: v.to_string(),
                    },
                };

                if cmd.output {
                    let postgres = PostgresStdout::default();
                    let task = FullRestoreTask::new(postgres, bridge, options);
                    let _ = task.run(|_, _| {})?; // do not display the progress bar
                    return Ok(());
                }

                match destination.connection_uri()? {
                    ConnectionUri::Postgres(host, port, username, password, database) => {
                        let postgres = DestinationPostgres::new(
                            host.as_str(),
                            port,
                            database.as_str(),
                            username.as_str(),
                            password.as_str(),
                            true,
                        );

                        let task = FullRestoreTask::new(postgres, bridge, options);
                        task.run(progress_callback)?
                    }
                    ConnectionUri::Mysql(host, port, username, password, database) => {
                        todo!() // FIXME
                    }
                    ConnectionUri::MongoDB(
                        host,
                        port,
                        username,
                        password,
                        database,
                        authentication_db,
                    ) => {
                        let mongodb = DestinationMongoDB::new(
                            host.as_str(),
                            port,
                            database.as_str(),
                            username.as_str(),
                            password.as_str(),
                            authentication_db.as_str(),
                        );

                        let task = FullRestoreTask::new(mongodb, bridge, options);
                        task.run(progress_callback)?
                    }
                }

                println!("Restore successful!")
            }
            None => {
                return Err(anyhow::Error::from(Error::new(
                    ErrorKind::Other,
                    "missing <destination> object in the configuration file",
                )));
            }
        },
    };

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_from_postgres() {}
}
