use std::marker::PhantomData;

use anyhow::{bail, Result};
use aqpbm_core::harness::engine::EngineUnderTest;
use aqpbm_core::harness::fake::FakeEngine;
use aqpbm_core::harness::one_shot::OneShotQueryEngine;
use aqpbm_core::harness::protocol::Protocol;
use clap::{ArgMatches, Args, Command, FromArgMatches};

use crate::engines_external::asapquery_backend_server::{self, AsapqueryBackendOptions};
use crate::engines_external::asapquery_precompute_engine_server::{
    self, AsapqueryPrecomputeEngineOptions,
};
use crate::engines_external::clickhouse_query_text::{self, ClickHouseQueryTextOptions};
use crate::engines_external::clickhouse_server::{self, ClickHouseServerOptions};
use crate::engines_external::druid_sql::{self, DruidOptions};
use crate::engines_external::duckdb_command_line::{self, DuckDbOptions};
use crate::engines_external::pilotdb::{self, PilotDbOptions};
use crate::engines_external::postgresql_command_line::{self, PostgreSqlOptions};
use crate::engines_external::victoriametrics_server::{self, VictoriaMetricsOptions};

pub struct System {
    pub name: &'static str,
    pub periodic_queries: Option<PeriodicQueries>,
    pub write_throughput: Option<WriteThroughput>,
    pub one_shot_queries: Option<OneShotQueries>,
}

pub struct PeriodicQueries {
    pub options: fn(Command) -> Command,
    pub build: fn(&ArgMatches, &Protocol) -> Result<Box<dyn EngineUnderTest>>,
}

pub struct WriteThroughput {
    pub options: fn(Command) -> Command,
    pub build: fn(&ArgMatches) -> Result<Box<dyn EngineUnderTest>>,
}

pub struct OneShotQueries {
    pub dialect: &'static str,
    pub options: fn(Command) -> Command,
    pub build: fn(&ArgMatches) -> Result<Box<dyn OneShotQueryEngine>>,
}

pub const SYSTEMS: &[System] = &[
    System {
        name: "fake",
        periodic_queries: Some(PeriodicQueries {
            options: no_options,
            build: fake_engine,
        }),
        write_throughput: None,
        one_shot_queries: None,
    },
    System {
        name: "duckdb",
        periodic_queries: None,
        write_throughput: None,
        one_shot_queries: Some(OneShotQueries {
            dialect: duckdb_command_line::DIALECT,
            options: DuckDbOptions::augment_args,
            build: duckdb_command_line::one_shot_queries,
        }),
    },
    System {
        name: "clickhouse",
        periodic_queries: Some(PeriodicQueries {
            options: ClickHouseServerOptions::augment_args,
            build: clickhouse_server::periodic_queries,
        }),
        write_throughput: None,
        one_shot_queries: Some(OneShotQueries {
            dialect: clickhouse_query_text::DIALECT,
            options: ClickHouseQueryTextOptions::augment_args,
            build: clickhouse_query_text::one_shot_queries,
        }),
    },
    System {
        name: "victoriametrics",
        periodic_queries: Some(PeriodicQueries {
            options: VictoriaMetricsOptions::augment_args,
            build: victoriametrics_server::periodic_queries,
        }),
        write_throughput: None,
        one_shot_queries: None,
    },
    System {
        name: "asapquery-backend",
        periodic_queries: Some(PeriodicQueries {
            options: AsapqueryBackendOptions::augment_args,
            build: asapquery_backend_server::periodic_queries,
        }),
        write_throughput: None,
        one_shot_queries: None,
    },
    System {
        name: "asapquery-precompute-engine",
        periodic_queries: None,
        write_throughput: Some(WriteThroughput {
            options: AsapqueryPrecomputeEngineOptions::augment_args,
            build: asapquery_precompute_engine_server::write_throughput,
        }),
        one_shot_queries: None,
    },
    System {
        name: "pilotdb",
        periodic_queries: None,
        write_throughput: None,
        one_shot_queries: Some(OneShotQueries {
            dialect: duckdb_command_line::DIALECT,
            options: PilotDbOptions::augment_args,
            build: pilotdb::one_shot_queries,
        }),
    },
    System {
        name: "postgresql",
        periodic_queries: None,
        write_throughput: None,
        one_shot_queries: Some(OneShotQueries {
            dialect: postgresql_command_line::DIALECT,
            options: PostgreSqlOptions::augment_args,
            build: postgresql_command_line::one_shot_queries,
        }),
    },
    System {
        name: "druid",
        periodic_queries: None,
        write_throughput: None,
        one_shot_queries: Some(OneShotQueries {
            dialect: druid_sql::DIALECT,
            options: DruidOptions::augment_args,
            build: druid_sql::one_shot_queries,
        }),
    },
];

pub trait Measurement: 'static {
    fn of(system: &'static System) -> Option<&'static Self>;
    fn options(&self) -> fn(Command) -> Command;
}

impl Measurement for PeriodicQueries {
    fn of(system: &'static System) -> Option<&'static Self> {
        system.periodic_queries.as_ref()
    }
    fn options(&self) -> fn(Command) -> Command {
        self.options
    }
}

impl Measurement for WriteThroughput {
    fn of(system: &'static System) -> Option<&'static Self> {
        system.write_throughput.as_ref()
    }
    fn options(&self) -> fn(Command) -> Command {
        self.options
    }
}

impl Measurement for OneShotQueries {
    fn of(system: &'static System) -> Option<&'static Self> {
        system.one_shot_queries.as_ref()
    }
    fn options(&self) -> fn(Command) -> Command {
        self.options
    }
}

pub fn names<M: Measurement>() -> Vec<&'static str> {
    SYSTEMS
        .iter()
        .filter(|system| M::of(system).is_some())
        .map(|system| system.name)
        .collect()
}

pub fn dialects() -> Vec<&'static str> {
    let mut dialects: Vec<&'static str> = Vec::new();
    for measurement in SYSTEMS.iter().filter_map(OneShotQueries::of) {
        if !dialects.contains(&measurement.dialect) {
            dialects.push(measurement.dialect);
        }
    }
    dialects
}

pub fn find<M: Measurement>(name: &str) -> Option<&'static M> {
    SYSTEMS
        .iter()
        .filter(|system| system.name == name)
        .find_map(M::of)
}

pub struct SystemOptions<M> {
    pub matches: ArgMatches,
    measurement: PhantomData<fn() -> M>,
}

impl<M> std::fmt::Debug for SystemOptions<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.matches.fmt(formatter)
    }
}

impl<M: Measurement> FromArgMatches for SystemOptions<M> {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        Ok(SystemOptions {
            matches: matches.clone(),
            measurement: PhantomData,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        self.matches = matches.clone();
        Ok(())
    }
}

impl<M: Measurement> Args for SystemOptions<M> {
    fn augment_args(mut command: Command) -> Command {
        for system in SYSTEMS {
            let Some(measurement) = M::of(system) else {
                continue;
            };
            let declared = (measurement.options())(Command::new(system.name));
            for option in declared.get_arguments() {
                if command
                    .get_arguments()
                    .all(|known| known.get_id() != option.get_id())
                {
                    command = command.arg(option.clone().display_order(None));
                }
            }
        }
        command
    }

    fn augment_args_for_update(command: Command) -> Command {
        Self::augment_args(command)
    }
}

fn no_options(command: Command) -> Command {
    command
}

fn fake_engine(matches: &ArgMatches, _: &Protocol) -> Result<Box<dyn EngineUnderTest>> {
    let given = |id: &str| matches!(matches.try_get_raw(id), Ok(Some(_)));
    if [
        "clickhouse_url",
        "asapquery_backend_url",
        "victoriametrics_url",
        "victoriametrics_stream_aggregation_config",
        "server_cgroup_directory",
        "server_ssh_host",
    ]
    .into_iter()
    .any(given)
    {
        bail!(
            "--clickhouse-url, --victoriametrics-url, --victoriametrics-stream-aggregation-config, --asapquery-backend-url, --server-cgroup-directory and --server-ssh-host are for the server engines only"
        );
    }
    Ok(Box::new(FakeEngine::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared_twice_alike<M: Measurement>() {
        let mut seen: Vec<(&str, clap::Arg)> = Vec::new();
        for system in SYSTEMS {
            let Some(measurement) = M::of(system) else {
                continue;
            };
            let declared = (measurement.options())(Command::new(system.name));
            for option in declared.get_arguments() {
                match seen
                    .iter()
                    .find(|(_, known)| known.get_id() == option.get_id())
                {
                    Some((first, known)) => assert_eq!(
                        format!(
                            "{:?}",
                            (
                                known.get_long(),
                                known.get_default_values(),
                                known.get_action()
                            )
                        ),
                        format!(
                            "{:?}",
                            (
                                option.get_long(),
                                option.get_default_values(),
                                option.get_action()
                            )
                        ),
                        "{} and {} declare {} differently",
                        first,
                        system.name,
                        option.get_id()
                    ),
                    None => seen.push((system.name, option.clone())),
                }
            }
        }
        SystemOptions::<M>::augment_args(Command::new("measurement")).debug_assert();
    }

    #[test]
    fn an_option_two_systems_share_is_declared_alike() {
        declared_twice_alike::<PeriodicQueries>();
        declared_twice_alike::<WriteThroughput>();
        declared_twice_alike::<OneShotQueries>();
    }

    #[test]
    fn every_name_is_one_system() {
        for (index, system) in SYSTEMS.iter().enumerate() {
            assert!(
                SYSTEMS[..index]
                    .iter()
                    .all(|earlier| earlier.name != system.name),
                "{}",
                system.name
            );
        }
    }
}
