//! The `route` transform - splits event streams into multiple outputs.
//!
//! This transform demonstrates `SyncTransform` with multiple named outputs. It evaluates
//! conditions and sends events to different downstream paths based on which conditions match.
//!
//! # Why This Is a SyncTransform
//!
//! The route transform uses `SyncTransform` because it needs to write to multiple named outputs:
//! - Each route has a named output (e.g., "errors", "warnings")
//! - Plus an optional "_unmatched" output for events that match no route
//!
//! `FunctionTransform` can only write to a single output, so we need `SyncTransform`.
//!
//! # Configuration Example
//!
//! ```yaml
//! transforms:
//!   my_router:
//!     type: route
//!     inputs: ["my_source"]
//!     route:
//!       errors:
//!         type: vrl
//!         source: '.level == "error"'
//!       warnings:
//!         type: vrl
//!         source: '.level == "warn"'
//!
//! sinks:
//!   error_sink:
//!     type: pagerduty
//!     inputs: ["my_router.errors"]  # Note the dotted syntax
//!
//!   warning_sink:
//!     type: slack
//!     inputs: ["my_router.warnings"]
//!
//!   other_sink:
//!     type: elasticsearch
//!     inputs: ["my_router._unmatched"]  # Events that matched no route
//! ```
//!
//! # Multiple Matches
//!
//! An event can match MULTIPLE routes. Each matching route gets a copy:
//!
//! ```yaml
//! route:
//!   has_user:
//!     type: vrl
//!     source: 'exists(.user_id)'
//!   has_timestamp:
//!     type: vrl
//!     source: 'exists(.timestamp)'
//! ```
//!
//! An event with both `.user_id` and `.timestamp` would be sent to BOTH outputs.
//!
//! # The _unmatched Output
//!
//! By default (`reroute_unmatched: true`), events that match NO routes go to the
//! `_unmatched` output. This prevents accidental data loss.
//!
//! Set `reroute_unmatched: false` to silently discard unmatched events.

use indexmap::IndexMap;
use vector_lib::{
    config::clone_input_definitions, configurable::configurable_component, transform::SyncTransform,
};

use crate::{
    conditions::{AnyCondition, Condition, ConditionConfig, VrlConfig},
    config::{
        DataType, GenerateConfig, Input, OutputId, TransformConfig, TransformContext,
        TransformOutput,
    },
    event::Event,
    schema,
    transforms::Transform,
};

/// The reserved name for the unmatched output.
///
/// Events that don't match any defined route go here (when `reroute_unmatched` is true).
/// This name is reserved - users can't create a route named "_unmatched".
pub(crate) const UNMATCHED_ROUTE: &str = "_unmatched";

/// The runtime route transform.
///
/// This struct implements `SyncTransform` and holds:
/// - Compiled conditions for each route
/// - Configuration for whether to route unmatched events
///
/// # Cloning
///
/// When `enable_concurrency()` returns true, the topology clones this struct
/// for each parallel worker. Each condition must be safely clonable.
#[derive(Clone)]
pub struct Route {
    /// Ordered list of (route_name, condition) pairs.
    ///
    /// Using a Vec instead of HashMap preserves the order from the config file.
    /// This matters when an event could match multiple routes - all matching
    /// routes receive the event.
    conditions: Vec<(String, Condition)>,

    /// Whether to send unmatched events to the "_unmatched" output.
    ///
    /// When false, unmatched events are silently dropped.
    reroute_unmatched: bool,
}

impl Route {
    /// Builds the runtime router from configuration.
    ///
    /// This compiles each route's condition from config to executable form.
    pub fn new(config: &RouteConfig, context: &TransformContext) -> crate::Result<Self> {
        let mut conditions = Vec::with_capacity(config.route.len());

        // Compile each route's condition
        for (output_name, condition) in config.route.iter() {
            let condition =
                condition.build(&context.enrichment_tables, &context.metrics_storage)?;
            conditions.push((output_name.clone(), condition));
        }

        Ok(Self {
            conditions,
            reroute_unmatched: config.reroute_unmatched,
        })
    }
}

/// The `SyncTransform` implementation - where routing decisions happen.
///
/// This is the hot path that gets called for every event. The implementation:
/// 1. Evaluates each route's condition against the event
/// 2. Pushes to each matching route's output
/// 3. If no routes matched and reroute_unmatched is true, pushes to "_unmatched"
///
/// # Multiple Matches
///
/// An event can be pushed to MULTIPLE outputs. This is intentional - it allows
/// the same event to flow down multiple paths:
///
/// ```ignore
/// // Event: { "level": "error", "has_pii": true }
///
/// // Routes:
/// // - errors: .level == "error"
/// // - sensitive: .has_pii == true
///
/// // Result: Event goes to BOTH "errors" AND "sensitive" outputs
/// ```
impl SyncTransform for Route {
    fn transform(&mut self, event: Event, output: &mut vector_lib::transform::TransformOutputsBuf) {
        let mut check_failed: usize = 0;

        // Check each route condition
        for (output_name, condition) in &self.conditions {
            // Clone the event for each check since conditions may need ownership
            let (result, event) = condition.check(event.clone());

            if result {
                // Route matched: send to this output
                output.push(Some(output_name), event);
            } else {
                check_failed += 1;
            }
        }

        // If ALL routes failed and reroute_unmatched is enabled, send to _unmatched
        if self.reroute_unmatched && check_failed == self.conditions.len() {
            output.push(Some(UNMATCHED_ROUTE), event);
        }
        // Otherwise: event is dropped (no push)
    }
}

/// Configuration for the `route` transform.
///
/// This struct deserializes from YAML and defines:
/// - Whether to reroute unmatched events
/// - The map of route names to conditions
///
/// # Reserved Names
///
/// The following route names are reserved and will cause validation errors:
/// - `_unmatched`: Reserved for the unmatched output
/// - `_default`: Reserved for future use
///
/// # Example
///
/// ```yaml
/// type: route
/// reroute_unmatched: true
/// route:
///   errors:
///     type: vrl
///     source: '.level == "error"'
///   warnings:
///     type: vrl
///     source: '.level in ["warn", "warning"]'
/// ```
#[configurable_component(transform(
    "route",
    "Split a stream of events into multiple sub-streams based on user-supplied conditions."
))]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    /// Reroutes unmatched events to a named output instead of silently discarding them.
    ///
    /// Normally, if an event doesn't match any defined route, it is sent to the `<transform_name>._unmatched`
    /// output for further processing. In some cases, you may want to simply discard unmatched events and not
    /// process them any further.
    ///
    /// In these cases, `reroute_unmatched` can be set to `false` to disable the `<transform_name>._unmatched`
    /// output and instead silently discard any unmatched events.
    #[serde(default = "crate::serde::default_true")]
    #[configurable(metadata(docs::human_name = "Reroute Unmatched Events"))]
    reroute_unmatched: bool,

    /// A map from route identifiers to logical conditions.
    /// Each condition represents a filter which is applied to each event.
    ///
    /// The following identifiers are reserved output names and thus cannot be used as route IDs:
    /// - `_unmatched`
    /// - `_default`
    ///
    /// Each route can then be referenced as an input by other components with the name
    /// `<transform_name>.<route_id>`. If an event doesn't match any route, and if `reroute_unmatched`
    /// is set to `true` (the default), it is sent to the `<transform_name>._unmatched` output.
    /// Otherwise, the unmatched event is instead silently discarded.
    #[configurable(metadata(docs::additional_props_description = "An individual route."))]
    #[configurable(metadata(docs::examples = "route_examples()"))]
    route: IndexMap<String, AnyCondition>,
}

/// Generates example route configuration for documentation.
fn route_examples() -> IndexMap<String, AnyCondition> {
    IndexMap::from([
        (
            "foo-exists".to_owned(),
            AnyCondition::Map(ConditionConfig::Vrl(VrlConfig {
                source: "exists(.foo)".to_owned(),
                ..Default::default()
            })),
        ),
        (
            "foo-does-not-exist".to_owned(),
            AnyCondition::Map(ConditionConfig::Vrl(VrlConfig {
                source: "!exists(.foo)".to_owned(),
                ..Default::default()
            })),
        ),
    ])
}

impl GenerateConfig for RouteConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            reroute_unmatched: true,
            route: route_examples(),
        })
        .unwrap()
    }
}

/// Implementation of `TransformConfig` for route.
///
/// This implementation:
/// 1. Validates that no route uses reserved names
/// 2. Declares outputs for each route plus optional _unmatched
/// 3. Enables parallelization (routing is stateless)
#[async_trait::async_trait]
#[typetag::serde(name = "route")]
impl TransformConfig for RouteConfig {
    /// Builds the runtime route transform.
    ///
    /// Creates a `Route` struct and wraps it in `Transform::synchronous()`.
    /// We use synchronous (not function) because we have multiple named outputs.
    async fn build(&self, context: &TransformContext) -> crate::Result<Transform> {
        let route = Route::new(self, context)?;
        Ok(Transform::synchronous(route))
    }

    /// Declares that route accepts all event types.
    ///
    /// Routing conditions can check any event type. A VRL condition like
    /// `exists(.level)` works on logs but just returns false on metrics.
    fn input(&self) -> Input {
        Input::all()
    }

    /// Validates that no route uses reserved names.
    ///
    /// This is called during config validation, before building.
    /// Returns all validation errors at once for better UX.
    fn validate(&self, _: &schema::Definition) -> Result<(), Vec<String>> {
        if self.route.contains_key(UNMATCHED_ROUTE) {
            Err(vec![format!(
                "cannot have a named output with reserved name: `{UNMATCHED_ROUTE}`"
            )])
        } else {
            Ok(())
        }
    }

    /// Declares the outputs for this transform.
    ///
    /// For each route in the config, we create a named output with that route's name.
    /// If `reroute_unmatched` is true, we also create an "_unmatched" output.
    ///
    /// Each output:
    /// - Passes through events unchanged (same schema as input)
    /// - Has a named port matching the route name
    fn outputs(
        &self,
        _: &TransformContext,
        input_definitions: &[(OutputId, schema::Definition)],
    ) -> Vec<TransformOutput> {
        // Create an output for each named route
        let mut result: Vec<TransformOutput> = self
            .route
            .keys()
            .map(|output_name| {
                TransformOutput::new(
                    DataType::all_bits(),
                    clone_input_definitions(input_definitions),
                )
                .with_port(output_name)
            })
            .collect();

        // Optionally add the _unmatched output
        if self.reroute_unmatched {
            result.push(
                TransformOutput::new(
                    DataType::all_bits(),
                    clone_input_definitions(input_definitions),
                )
                .with_port(UNMATCHED_ROUTE),
            );
        }
        result
    }

    /// Enables parallel execution for better throughput.
    ///
    /// Routing is stateless (each event is evaluated independently), so it's
    /// safe to parallelize. Multiple workers can evaluate routes concurrently.
    fn enable_concurrency(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use indoc::indoc;
    use vector_lib::{config::LogNamespace, transform::TransformOutputsBuf};

    use super::*;
    use crate::{
        config::{ConfigBuilder, build_unit_tests},
        test_util::components::{COMPONENT_MULTIPLE_OUTPUTS_TESTS, init_test},
    };

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<super::RouteConfig>();
    }

    #[test]
    fn can_serialize_remap() {
        // We need to serialize the config to check if a config has
        // changed when reloading.
        let config = toml::from_str::<RouteConfig>(
            r#"
            route.first.type = "vrl"
            route.first.source = '.message == "hello world"'
        "#,
        )
        .unwrap();

        assert_eq!(
            serde_json::to_string(&config).unwrap(),
            r#"{"reroute_unmatched":true,"route":{"first":{"type":"vrl","source":".message == \"hello world\""}}}"#
        );
    }

    #[test]
    fn route_pass_all_route_conditions() {
        let output_names = vec!["first", "second", "third", UNMATCHED_ROUTE];
        let event = Event::from_json_value(
            serde_json::json!({"message": "hello world", "second": "second", "third": "third"}),
            LogNamespace::Legacy,
        )
        .unwrap();
        let config = toml::from_str::<RouteConfig>(
            r#"
            route.first.type = "vrl"
            route.first.source = '.message == "hello world"'

            route.second.type = "vrl"
            route.second.source = '.second == "second"'

            route.third.type = "vrl"
            route.third.source = '.third == "third"'
        "#,
        )
        .unwrap();

        let mut transform = Route::new(&config, &Default::default()).unwrap();
        let mut outputs = TransformOutputsBuf::new_with_capacity(
            output_names
                .iter()
                .map(|output_name| {
                    TransformOutput::new(DataType::all_bits(), HashMap::new())
                        .with_port(output_name.to_owned())
                })
                .collect(),
            1,
        );

        transform.transform(event.clone(), &mut outputs);
        for output_name in output_names {
            let mut events: Vec<_> = outputs.drain_named(output_name).collect();
            if output_name == UNMATCHED_ROUTE {
                assert!(events.is_empty());
            } else {
                assert_eq!(events.len(), 1);
                assert_eq!(events.pop().unwrap(), event);
            }
        }
    }

    #[test]
    fn route_pass_one_route_condition() {
        let output_names = vec!["first", "second", "third", UNMATCHED_ROUTE];
        let event = Event::from_json_value(
            serde_json::json!({"message": "hello world"}),
            LogNamespace::Legacy,
        )
        .unwrap();
        let config = toml::from_str::<RouteConfig>(
            r#"
            route.first.type = "vrl"
            route.first.source = '.message == "hello world"'

            route.second.type = "vrl"
            route.second.source = '.second == "second"'

            route.third.type = "vrl"
            route.third.source = '.third == "third"'
        "#,
        )
        .unwrap();

        let mut transform = Route::new(&config, &Default::default()).unwrap();
        let mut outputs = TransformOutputsBuf::new_with_capacity(
            output_names
                .iter()
                .map(|output_name| {
                    TransformOutput::new(DataType::all_bits(), HashMap::new())
                        .with_port(output_name.to_owned())
                })
                .collect(),
            1,
        );

        transform.transform(event.clone(), &mut outputs);
        for output_name in output_names {
            let mut events: Vec<_> = outputs.drain_named(output_name).collect();
            if output_name == "first" {
                assert_eq!(events.len(), 1);
                assert_eq!(events.pop().unwrap(), event);
            }
            assert_eq!(events.len(), 0);
        }
    }

    #[test]
    fn route_pass_no_route_condition() {
        let output_names = vec!["first", "second", "third", UNMATCHED_ROUTE];
        let event =
            Event::from_json_value(serde_json::json!({"message": "NOPE"}), LogNamespace::Legacy)
                .unwrap();
        let config = toml::from_str::<RouteConfig>(
            r#"
            route.first.type = "vrl"
            route.first.source = '.message == "hello world"'

            route.second.type = "vrl"
            route.second.source = '.second == "second"'

            route.third.type = "vrl"
            route.third.source = '.third == "third"'
        "#,
        )
        .unwrap();

        let mut transform = Route::new(&config, &Default::default()).unwrap();
        let mut outputs = TransformOutputsBuf::new_with_capacity(
            output_names
                .iter()
                .map(|output_name| {
                    TransformOutput::new(DataType::all_bits(), HashMap::new())
                        .with_port(output_name.to_owned())
                })
                .collect(),
            1,
        );

        transform.transform(event.clone(), &mut outputs);
        for output_name in output_names {
            let mut events: Vec<_> = outputs.drain_named(output_name).collect();
            if output_name == UNMATCHED_ROUTE {
                assert_eq!(events.len(), 1);
                assert_eq!(events.pop().unwrap(), event);
            }
            assert_eq!(events.len(), 0);
        }
    }

    #[test]
    fn route_no_unmatched_output() {
        let output_names = vec!["first", "second", "third", UNMATCHED_ROUTE];
        let event =
            Event::from_json_value(serde_json::json!({"message": "NOPE"}), LogNamespace::Legacy)
                .unwrap();
        let config = toml::from_str::<RouteConfig>(
            r#"
            reroute_unmatched = false

            route.first.type = "vrl"
            route.first.source = '.message == "hello world"'

            route.second.type = "vrl"
            route.second.source = '.second == "second"'

            route.third.type = "vrl"
            route.third.source = '.third == "third"'
        "#,
        )
        .unwrap();

        let mut transform = Route::new(&config, &Default::default()).unwrap();
        let mut outputs = TransformOutputsBuf::new_with_capacity(
            output_names
                .iter()
                .map(|output_name| {
                    TransformOutput::new(DataType::all_bits(), HashMap::new())
                        .with_port(output_name.to_owned())
                })
                .collect(),
            1,
        );

        transform.transform(event.clone(), &mut outputs);
        for output_name in output_names {
            let events: Vec<_> = outputs.drain_named(output_name).collect();
            assert_eq!(events.len(), 0);
        }
    }

    #[tokio::test]
    async fn route_metrics_with_output_tag() {
        init_test();

        let config: ConfigBuilder = toml::from_str(indoc! {r#"
            [transforms.foo]
            inputs = []
            type = "route"
            [transforms.foo.route.first]
                type = "is_log"

            [[tests]]
            name = "metric output"

            [tests.input]
                insert_at = "foo"
                value = "none"

            [[tests.outputs]]
                extract_from = "foo.first"
                [[tests.outputs.conditions]]
                type = "vrl"
                source = "true"
        "#})
        .unwrap();

        let mut tests = build_unit_tests(config).await.unwrap();
        assert!(tests.remove(0).run().await.errors.is_empty());
        // Check that metrics were emitted with output tag
        COMPONENT_MULTIPLE_OUTPUTS_TESTS.assert(&["output"]);
    }
}
