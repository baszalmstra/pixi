use std::{collections::BTreeMap, sync::Arc};

use minijinja::Value;
use ordermap::OrderMap;
use pixi_build_types::{
    BinaryPackageSpec, ExtraGroupName, PackageSpec, PinBound, PinCompatibleSpec, SourcePackageName,
    SourcePackageSpec, Target, Targets,
    procedures::conda_build_v1::{
        CondaBuildV1Dependency, CondaBuildV1DependencySource, CondaBuildV1Prefix,
        CondaBuildV1RunExports,
    },
};
use rattler_build_core::render::resolved_dependencies::{
    DependencyInfo, FinalizedDependencies, FinalizedRunDependencies, ResolvedDependencies,
    RunExportDependency, SourceDependency,
};
use rattler_build_jinja::Variable;
use rattler_build_recipe::stage0::{
    Conditional, ConditionalList, Item, JinjaExpression, JinjaTemplate, NestedItemList,
    Requirements, SerializableMatchSpec, Value as RecipeValue,
};

use crate::package_dependency::{PackageDependency, SourceMatchSpec};
use miette::Diagnostic;
use rattler_conda_types::{
    Channel, MatchSpec, PackageName, PackageNameMatcher, package::RunExportsJson,
};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::encoded_source_spec_url::EncodedSourceSpecUrl;

#[derive(Debug, Error, Diagnostic)]
pub enum SelectorConversionError {
    #[error("invalid selector expression `{expression}`: {message}")]
    InvalidExpression { expression: String, message: String },
    #[error("{0}")]
    SpecConversion(SpecItemConversionError),
}

/// Wraps the `miette::Report` produced by [`package_spec_to_item`] (and its
/// callees) so it can be threaded through [`SelectorConversionError`] instead
/// of being silently `.unwrap()`-ed. `miette::Report` does not itself
/// implement `std::error::Error`, so its message is captured as a plain
/// `String` rather than relying on `#[from]`/`#[source]` error chaining.
#[derive(Debug, Error, Diagnostic)]
#[error("{0}")]
pub struct SpecItemConversionError(String);

impl From<miette::Report> for SpecItemConversionError {
    fn from(value: miette::Report) -> Self {
        SpecItemConversionError(format!("{value:?}"))
    }
}

impl From<SpecItemConversionError> for SelectorConversionError {
    fn from(value: SpecItemConversionError) -> Self {
        SelectorConversionError::SpecConversion(value)
    }
}

pub fn from_source_url_to_source_package(source_url: Url) -> Option<SourcePackageSpec> {
    match source_url.scheme() {
        "source" => Some(EncodedSourceSpecUrl::from(source_url).into()),
        _ => None,
    }
}

pub fn from_source_matchspec_into_package_spec(
    source_matchspec: SourceMatchSpec,
) -> miette::Result<SourcePackageSpec> {
    from_source_url_to_source_package(source_matchspec.location)
        .ok_or_else(|| miette::miette!("Only file, http/https and git are supported for now"))
}

pub fn convert_variant_from_pixi_build_types(variant: pixi_build_types::VariantValue) -> Variable {
    match variant {
        pixi_build_types::VariantValue::String(s) => Variable::from(s),
        pixi_build_types::VariantValue::Int(i) => Variable::from(i),
        pixi_build_types::VariantValue::Bool(b) => Variable::from(b),
    }
}

pub fn convert_variant_to_pixi_build_types(
    variant: Variable,
) -> Result<pixi_build_types::VariantValue, minijinja::Error> {
    let value = Value::from(variant);
    pixi_build_types::VariantValue::deserialize(value)
}

/// Convert a `PackageDependency` to a `SerializableMatchSpec` for use in
/// rattler-build's `Requirements`.
fn package_dependency_to_matchspec(dep: PackageDependency) -> SerializableMatchSpec {
    dep.into()
}

/// Convert a `PackageDependency` into an `Item<SerializableMatchSpec>`.
fn package_dependency_to_item(dep: PackageDependency) -> Item<SerializableMatchSpec> {
    Item::Value(RecipeValue::new_concrete(
        package_dependency_to_matchspec(dep),
        None,
    ))
}

/// Renders `s` as a single-quoted Jinja/Python string literal, escaping
/// backslashes and single quotes so that values containing either (a stray
/// `'` in a build-string matcher, for example) cannot break out of the
/// literal and corrupt the generated `pin_subpackage(...)` call.
fn jinja_string_literal(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// Renders a single bound (`lower_bound`/`upper_bound`) of a [`PinCompatibleSpec`]
/// as a `pin_subpackage(...)` keyword argument value: a quoted pin expression
/// (`"x.x.x"`) or a quoted literal version, matching rattler-build's
/// `Pin`/`pin_subpackage()` Jinja helper signature.
fn pin_bound_arg(bound: &PinBound) -> String {
    match bound {
        PinBound::Expression(expr) => jinja_string_literal(&expr.0),
        PinBound::Version(version) => jinja_string_literal(&version.to_string()),
    }
}

/// Render a [`PackageSpec::PinCompatible`] self-pin as a literal Jinja
/// `${{ pin_subpackage('<name>', ...) }}` template item, mirroring
/// `compilers.rs`'s `${{ compiler('<lang>') }}` pattern exactly. The
/// rendering happens through the *existing* rattler-build-recipe Jinja
/// `pin_subpackage()` mechanism (the same one `recipe.yaml` authors already
/// use), so resolution against the current output's own
/// `(name, version, build_string)` is handled entirely downstream, by code
/// that is already correct and already tested for the `recipe.yaml` path.
///
/// Boolean casing (`exact=true`, lowercase) is confirmed against the literal
/// fixture at `tests/recipe/pin-subpackage/recipe.yaml`.
fn pin_subpackage_item(name: &str, pin: &PinCompatibleSpec) -> Item<SerializableMatchSpec> {
    let mut args = vec![jinja_string_literal(name)];
    if let Some(lower_bound) = &pin.lower_bound {
        args.push(format!("lower_bound={}", pin_bound_arg(lower_bound)));
    }
    if let Some(upper_bound) = &pin.upper_bound {
        args.push(format!("upper_bound={}", pin_bound_arg(upper_bound)));
    }
    if pin.exact {
        args.push("exact=true".to_string());
    }
    if let Some(build) = &pin.build {
        args.push(format!("build={}", jinja_string_literal(build)));
    }
    let template = JinjaTemplate::new(format!("${{{{ pin_subpackage({}) }}}}", args.join(", ")))
        .expect("valid jinja template");
    crate::compilers::template_item(template)
}

/// Convert a single `(name, PackageSpec)` pair into an
/// `Item<SerializableMatchSpec>`. `PinCompatible` specs render as a literal
/// `pin_subpackage(...)` Jinja template via [`pin_subpackage_item`];
/// `Binary`/`Source` specs go through the existing concrete
/// `PackageDependency` conversion.
fn package_spec_to_item(
    name: &SourcePackageName,
    spec: &PackageSpec,
) -> miette::Result<Item<SerializableMatchSpec>> {
    match spec {
        PackageSpec::PinCompatible(pin) => Ok(pin_subpackage_item(name.as_str(), pin)),
        _ => {
            let dep = package_spec_to_package_dependency(
                PackageName::new_unchecked(name.as_str()),
                spec.clone(),
            )?;
            Ok(package_dependency_to_item(dep))
        }
    }
}

/// Convert an ordered map of `(name, PackageSpec)` into `Item`s, preserving
/// order. Unlike [`package_specs_to_package_dependency`], this handles
/// `PackageSpec::PinCompatible` by emitting a Jinja template item instead of
/// erroring.
fn package_specs_to_items(
    specs: &OrderMap<SourcePackageName, PackageSpec>,
) -> miette::Result<Vec<Item<SerializableMatchSpec>>> {
    specs
        .iter()
        .map(|(name, spec)| package_spec_to_item(name, spec))
        .collect()
}

/// Accumulates the per-section requirement items while converting targets.
#[derive(Default)]
struct RequirementItems {
    build: ConditionalList<SerializableMatchSpec>,
    host: ConditionalList<SerializableMatchSpec>,
    run: ConditionalList<SerializableMatchSpec>,
    run_constraints: ConditionalList<SerializableMatchSpec>,
    extras: BTreeMap<String, ConditionalList<SerializableMatchSpec>>,
    /// Run-exports buckets, keyed the same way as `pbt::RunExportsTarget`'s
    /// fields (`noarch`/`strong`/`weak`/`strong_constrains`/`weak_constrains`).
    run_exports: RunExportItems,
}

/// Accumulates the five run-exports buckets while converting targets, mirroring
/// `pbt::RunExportsTarget`'s field names. `[package.run-exports.*]` is only
/// ever populated on the default (unconditional) target — enforced by the
/// manifest-layer parser (`TomlRunExports` has no `if(...)` sub-table support)
/// — so in practice `add_target`'s `condition` is always `None` whenever
/// `target.run_exports` is `Some`, making the `wrap()` call applied to these
/// items (the same helper used for every other bucket) a no-op.
#[derive(Default)]
struct RunExportItems {
    noarch: ConditionalList<SerializableMatchSpec>,
    strong: ConditionalList<SerializableMatchSpec>,
    weak: ConditionalList<SerializableMatchSpec>,
    strong_constrains: ConditionalList<SerializableMatchSpec>,
    weak_constrains: ConditionalList<SerializableMatchSpec>,
}

impl RequirementItems {
    /// Add the dependencies of `target`, wrapping each one in `condition` when
    /// one is given.
    fn add_target(
        &mut self,
        target: &Target,
        condition: Option<&JinjaExpression>,
    ) -> Result<(), SpecItemConversionError> {
        let wrap = |item: Item<SerializableMatchSpec>| -> Item<SerializableMatchSpec> {
            match condition {
                Some(condition) => Item::Conditional(Conditional {
                    condition: condition.clone(),
                    then: NestedItemList::single(item),
                    else_value: None,
                    condition_span: None,
                }),
                None => item,
            }
        };

        let to_items = |specs: Option<&OrderMap<SourcePackageName, PackageSpec>>| -> Result<
            Vec<Item<SerializableMatchSpec>>,
            SpecItemConversionError,
        > {
            let items = match specs {
                Some(specs) => package_specs_to_items(specs)?,
                None => Vec::new(),
            };
            Ok(items.into_iter().map(wrap).collect::<Vec<_>>())
        };

        self.build
            .extend(to_items(target.build_dependencies.as_ref())?);
        self.host
            .extend(to_items(target.host_dependencies.as_ref())?);
        self.run.extend(to_items(target.run_dependencies.as_ref())?);
        self.run_constraints
            .extend(to_items(target.run_constraints.as_ref())?);

        if let Some(target_extras) = &target.extra_dependencies {
            for (group, deps) in target_extras {
                let items = package_specs_to_items(deps)?.into_iter().map(wrap);
                self.extras
                    .entry(group.to_string())
                    .or_default()
                    .extend(items);
            }
        }

        if let Some(run_exports) = &target.run_exports {
            // `to_items` applies `wrap` like every other bucket, but since
            // `[package.run-exports.*]` is only ever populated on the default
            // (unconditional) target (see the `RunExportItems` doc comment),
            // `condition` is always `None` here in practice, so `wrap` is a
            // no-op.
            self.run_exports
                .noarch
                .extend(to_items(run_exports.noarch.as_ref())?);
            self.run_exports
                .strong
                .extend(to_items(run_exports.strong.as_ref())?);
            self.run_exports
                .weak
                .extend(to_items(run_exports.weak.as_ref())?);
            self.run_exports
                .strong_constrains
                .extend(to_items(run_exports.strong_constrains.as_ref())?);
            self.run_exports
                .weak_constrains
                .extend(to_items(run_exports.weak_constrains.as_ref())?);
        }

        Ok(())
    }
}

pub fn from_targets_v1_to_conditional_requirements(
    targets: &Targets,
) -> Result<Requirements, SelectorConversionError> {
    let mut items = RequirementItems::default();

    // Add default target
    if let Some(default_target) = &targets.default_target {
        items.add_target(default_target, None)?;
    }

    // Add conditional `if(...)` targets. The expression is handed to
    // rattler-build verbatim; pixi does not evaluate it.
    if let Some(conditional_targets) = &targets.conditional {
        for (expression, target) in conditional_targets {
            let condition = JinjaExpression::new(expression.to_string()).map_err(|message| {
                SelectorConversionError::InvalidExpression {
                    expression: expression.to_string(),
                    message,
                }
            })?;
            items.add_target(target, Some(&condition))?;
        }
    }

    let RequirementItems {
        build,
        host,
        run,
        run_constraints,
        extras,
        run_exports,
    } = items;
    let mut requirements = Requirements {
        build,
        host,
        run,
        run_constraints,
        extras,
        ..Default::default()
    };
    // `RunExports`'s containing module is private (`rattler_build_recipe::stage0::requirements`
    // is `mod`, not `pub mod`), so the type can't be named from this crate; its
    // fields are `pub`, so they can still be set through a `Default`-constructed
    // instance (which `Requirements::default()` already gave us above).
    requirements.run_exports.noarch = run_exports.noarch;
    requirements.run_exports.strong = run_exports.strong;
    requirements.run_exports.weak = run_exports.weak;
    requirements.run_exports.strong_constraints = run_exports.strong_constrains;
    requirements.run_exports.weak_constraints = run_exports.weak_constrains;
    Ok(requirements)
}

pub(crate) fn source_package_spec_to_package_dependency(
    name: PackageName,
    source_spec: SourcePackageSpec,
) -> miette::Result<SourceMatchSpec> {
    let spec = MatchSpec {
        name: PackageNameMatcher::Exact(name),
        ..Default::default()
    };

    Ok(SourceMatchSpec {
        spec,
        location: EncodedSourceSpecUrl::from(source_spec).into(),
    })
}

fn binary_package_spec_to_package_dependency(
    name: PackageName,
    binary_spec: BinaryPackageSpec,
) -> PackageDependency {
    let BinaryPackageSpec {
        version,
        build,
        build_number,
        file_name,
        extras,
        flags,
        channel,
        subdir,
        md5,
        sha256,
        url,
        license,
        condition,
    } = binary_spec;

    // If the version is "*" and no other constraints are present, treat it as None
    // so later rattler-build can detect the PackageDependency as a variant.
    // If other constraints (e.g. `build`) are present, the spec is not a variant
    // and we must keep `Some(Any)` so the resulting `MatchSpec` round-trips correctly
    // through its `Display`/`FromStr` representation.
    //
    // The destructure of `BinaryPackageSpec` above and the match below are
    // intentionally exhaustive: when a new field is added to `BinaryPackageSpec`,
    // the compiler forces us to revisit whether it should count as a constraint.
    let version = match (
        &build,
        &build_number,
        &file_name,
        &channel,
        &subdir,
        &md5,
        &sha256,
        &url,
        &license,
        &condition,
    ) {
        (None, None, None, None, None, None, None, None, None, None) => {
            version.filter(|v| v != &rattler_conda_types::VersionSpec::Any)
        }
        _ => Some(version.unwrap_or(rattler_conda_types::VersionSpec::Any)),
    };

    PackageDependency::Binary(MatchSpec {
        name: PackageNameMatcher::Exact(name),
        version,
        build,
        build_number,
        file_name,
        extras,
        channel: channel.map(Channel::from_url).map(Arc::new),
        subdir,
        namespace: None,
        md5,
        sha256,
        url,
        license,
        condition,
        // `track_features` and `license_family` are deprecated matchspec fields
        // that pixi does not propagate.
        track_features: None,
        flags,
        license_family: None,
    })
}

fn package_spec_to_package_dependency(
    name: PackageName,
    spec: PackageSpec,
) -> miette::Result<PackageDependency> {
    match spec {
        PackageSpec::Binary(binary_spec) => Ok(binary_package_spec_to_package_dependency(
            name,
            *binary_spec,
        )),
        PackageSpec::Source(source_spec) => Ok(PackageDependency::Source(
            source_package_spec_to_package_dependency(name, source_spec)?,
        )),
        PackageSpec::PinCompatible(_) => {
            miette::bail!("PinCompatible package specs are not yet supported in this context")
        }
    }
}

pub(crate) fn package_specs_to_package_dependency(
    specs: OrderMap<SourcePackageName, PackageSpec>,
) -> miette::Result<Vec<PackageDependency>> {
    specs
        .into_iter()
        .map(|(name, spec)| {
            package_spec_to_package_dependency(PackageName::new_unchecked(name.as_str()), spec)
        })
        .collect()
}

/// A helper struct for organizing dependencies by type.
///
/// **Note:** [`PackageDependency`] has no variant for `PackageSpec::PinCompatible`
/// (it only models concrete `Binary`/`Source` specs), so the `From<&Target>`
/// impl below panics if a target's dependencies contain a `PinCompatible`
/// entry (e.g. from a manifest's `pin-subpackage`). `RequirementItems::add_target`
/// (used by [`from_targets_v1_to_conditional_requirements`], the actual
/// manifest -> Jinja-recipe conversion path) does **not** go through this
/// type for that reason — it calls [`package_specs_to_items`] instead, which
/// renders `PinCompatible` as a Jinja template item via [`pin_subpackage_item`].
/// Prefer that path; this struct exists for callers that only ever see
/// resolved/concrete specs.
#[derive(Clone, Default)]
pub struct PackageSpecDependencies {
    pub build: indexmap::IndexMap<PackageName, PackageDependency>,
    pub host: indexmap::IndexMap<PackageName, PackageDependency>,
    pub run: indexmap::IndexMap<PackageName, PackageDependency>,
    pub run_constraints: indexmap::IndexMap<PackageName, PackageDependency>,
}

impl From<&Target> for PackageSpecDependencies {
    fn from(target: &Target) -> Self {
        let build_reqs = target
            .clone()
            .build_dependencies
            .map(|deps| package_specs_to_package_dependency(deps).unwrap())
            .unwrap_or_default();

        let host_reqs = target
            .clone()
            .host_dependencies
            .map(|deps| package_specs_to_package_dependency(deps).unwrap())
            .unwrap_or_default();

        let run_reqs = target
            .clone()
            .run_dependencies
            .map(|deps| package_specs_to_package_dependency(deps).unwrap())
            .unwrap_or_default();

        let run_constraints = target
            .clone()
            .run_constraints
            .map(|deps| package_specs_to_package_dependency(deps).unwrap())
            .unwrap_or_default();

        let mut bin_reqs = PackageSpecDependencies::default();

        for spec in build_reqs.iter() {
            if let Some(name) = spec.package_name() {
                bin_reqs.build.insert(name.clone(), spec.clone());
            }
        }

        for spec in host_reqs.iter() {
            if let Some(name) = spec.package_name() {
                bin_reqs.host.insert(name.clone(), spec.clone());
            }
        }

        for spec in run_reqs.iter() {
            if let Some(name) = spec.package_name() {
                bin_reqs.run.insert(name.clone(), spec.clone());
            }
        }

        for spec in run_constraints.iter() {
            if let Some(name) = spec.package_name() {
                bin_reqs.run_constraints.insert(name.clone(), spec.clone());
            }
        }

        bin_reqs
    }
}

pub(crate) fn from_build_v1_dependency_to_dependency_info(
    spec: CondaBuildV1Dependency,
) -> DependencyInfo {
    match spec.source {
        Some(CondaBuildV1DependencySource::RunExport(run_export)) => {
            DependencyInfo::RunExport(RunExportDependency {
                spec: spec.spec,
                from: run_export.from,
                source_package: run_export.package_name.as_normalized().to_string(),
            })
        }
        None => DependencyInfo::Source(SourceDependency { spec: spec.spec }),
    }
}

pub(crate) fn from_build_v1_run_exports_to_run_exports(
    run_exports: CondaBuildV1RunExports,
) -> RunExportsJson {
    RunExportsJson {
        weak: run_exports
            .weak
            .into_iter()
            .map(|dep| dep.spec.to_string())
            .collect(),
        strong: run_exports
            .strong
            .into_iter()
            .map(|dep| dep.spec.to_string())
            .collect(),
        noarch: run_exports
            .noarch
            .into_iter()
            .map(|dep| dep.spec.to_string())
            .collect(),
        strong_constrains: run_exports
            .strong_constrains
            .into_iter()
            .map(|dep| dep.spec.to_string())
            .collect(),
        weak_constrains: run_exports
            .weak_constrains
            .into_iter()
            .map(|dep| dep.spec.to_string())
            .collect(),
    }
}

pub fn from_build_v1_args_to_finalized_dependencies(
    build_prefix: Option<CondaBuildV1Prefix>,
    host_prefix: Option<CondaBuildV1Prefix>,
    run_dependencies: Option<Vec<CondaBuildV1Dependency>>,
    run_constraints: Option<Vec<CondaBuildV1Dependency>>,
    run_exports: Option<CondaBuildV1RunExports>,
    extra_dependencies: BTreeMap<ExtraGroupName, Vec<CondaBuildV1Dependency>>,
) -> FinalizedDependencies {
    FinalizedDependencies {
        build: build_prefix.map(|prefix| ResolvedDependencies {
            specs: prefix
                .dependencies
                .into_iter()
                .map(from_build_v1_dependency_to_dependency_info)
                .collect(),
            resolved: prefix
                .packages
                .into_iter()
                .map(|pkg| pkg.repodata_record)
                .collect(),
        }),
        host: host_prefix.map(|prefix| ResolvedDependencies {
            specs: prefix
                .dependencies
                .into_iter()
                .map(from_build_v1_dependency_to_dependency_info)
                .collect(),
            resolved: prefix
                .packages
                .into_iter()
                .map(|pkg| pkg.repodata_record)
                .collect(),
        }),
        run: FinalizedRunDependencies {
            depends: run_dependencies
                .unwrap_or_default()
                .into_iter()
                .map(from_build_v1_dependency_to_dependency_info)
                .collect(),
            constraints: run_constraints
                .unwrap_or_default()
                .into_iter()
                .map(from_build_v1_dependency_to_dependency_info)
                .collect(),
            extra_depends: extra_dependencies
                .into_iter()
                .map(|(group, deps)| {
                    (
                        group.into_inner(),
                        deps.into_iter()
                            .map(from_build_v1_dependency_to_dependency_info)
                            .collect(),
                    )
                })
                .collect(),
            run_exports: run_exports
                .map(from_build_v1_run_exports_to_run_exports)
                .unwrap_or_default(),
        },
    }
}

#[cfg(test)]
mod test {
    use pixi_build_types::ConditionalExpression;
    use rattler_conda_types::ParseMatchSpecOptions;

    use super::*;

    #[test]
    fn test_binary_package_conversion() {
        let name = PackageName::new_unchecked("foobar");
        let spec = BinaryPackageSpec {
            version: Some("3.12.*".parse().unwrap()),
            ..BinaryPackageSpec::default()
        };
        let match_spec = binary_package_spec_to_package_dependency(name, spec);
        assert_eq!(match_spec.to_string(), "foobar 3.12.*");
    }

    #[test]
    fn test_binary_package_conversion_any_is_treated_as_none() {
        let name = PackageName::new_unchecked("python");
        let spec = BinaryPackageSpec {
            version: Some("*".parse().unwrap()),
            ..BinaryPackageSpec::default()
        };
        let match_spec = binary_package_spec_to_package_dependency(name, spec);
        assert_eq!(match_spec.to_string(), "python");
    }

    #[test]
    fn test_binary_package_conversion_preserves_condition() {
        use rattler_conda_types::{MatchSpecCondition, ParseMatchSpecOptions, RepodataRevision};

        let name = PackageName::new_unchecked("numpy");
        let condition = MatchSpecCondition::MatchSpec(Box::new(
            MatchSpec::from_str(
                "python >=3.10",
                ParseMatchSpecOptions::lenient().with_repodata_revision(RepodataRevision::V3),
            )
            .unwrap(),
        ));
        let spec = BinaryPackageSpec {
            version: Some("*".parse().unwrap()),
            condition: Some(condition.clone()),
            ..BinaryPackageSpec::default()
        };
        let match_spec = binary_package_spec_to_package_dependency(name, spec);
        let PackageDependency::Binary(match_spec) = match_spec else {
            panic!("expected binary dependency");
        };
        assert_eq!(match_spec.condition, Some(condition));
    }

    #[test]
    fn test_extra_dependencies_are_finalized() {
        let dep = CondaBuildV1Dependency {
            spec: MatchSpec::from_str("gtest", ParseMatchSpecOptions::lenient()).unwrap(),
            source: None,
        };
        let mut extra = BTreeMap::new();
        extra.insert(ExtraGroupName::new("test").unwrap(), vec![dep]);

        let finalized =
            from_build_v1_args_to_finalized_dependencies(None, None, None, None, None, extra);

        let group = finalized
            .run
            .extra_depends
            .get("test")
            .expect("the `test` extra group must be finalized into run.extra_depends");
        assert_eq!(group.len(), 1);
        assert_eq!(group[0].spec().to_string(), "gtest");
    }

    #[test]
    fn test_extras_conversion() {
        // Top-level `[package.extra-dependencies.test]` lands on the default
        // target's extras, which should round-trip through
        // `from_targets_v1_to_conditional_requirements` as a bare
        // `gtest` value in the `test` group.
        let mut dependencies = OrderMap::new();
        dependencies.insert(
            SourcePackageName::from(PackageName::new_unchecked("gtest")),
            BinaryPackageSpec {
                version: Some("*".parse().unwrap()),
                ..BinaryPackageSpec::default()
            }
            .into(),
        );

        let mut extras = OrderMap::new();
        extras.insert(
            pixi_build_types::ExtraGroupName::new("test").unwrap(),
            dependencies,
        );

        let targets = Targets {
            default_target: Some(Target {
                extra_dependencies: Some(extras),
                ..Target::default()
            }),
            conditional: None,
        };
        let requirements = from_targets_v1_to_conditional_requirements(&targets).unwrap();
        let value = serde_json::to_value(&requirements.extras).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "test": ["gtest"]
            })
        );
    }

    /// A conditional `if(...)` dependency is wrapped in a `Conditional` carrying
    /// the user's expression verbatim.
    #[test]
    fn test_conditional_expression_passthrough() {
        let mut dependencies = OrderMap::new();
        dependencies.insert(
            SourcePackageName::from(PackageName::new_unchecked("foo")),
            BinaryPackageSpec {
                version: Some("*".parse().unwrap()),
                ..BinaryPackageSpec::default()
            }
            .into(),
        );

        let mut conditional = OrderMap::new();
        conditional.insert(
            ConditionalExpression::new("host_platform != build_platform"),
            Target {
                build_dependencies: Some(dependencies),
                ..Target::default()
            },
        );
        let targets = Targets {
            default_target: None,
            conditional: Some(conditional),
        };

        let requirements = from_targets_v1_to_conditional_requirements(&targets).unwrap();

        let value = serde_json::to_string(&requirements.build).unwrap();
        assert!(
            value.contains("host_platform != build_platform"),
            "conditional expression must be preserved verbatim: {value}"
        );
        assert!(
            value.contains("foo"),
            "conditional dependency must be present: {value}"
        );
    }

    /// A malformed user-supplied expression selector must surface as an error
    /// rather than panicking inside `JinjaExpression::new`.
    #[test]
    fn test_invalid_expression_selector_errors_instead_of_panicking() {
        let mut dependencies = OrderMap::new();
        dependencies.insert(
            SourcePackageName::from(PackageName::new_unchecked("foo")),
            BinaryPackageSpec {
                version: Some("*".parse().unwrap()),
                ..BinaryPackageSpec::default()
            }
            .into(),
        );

        let mut conditional = OrderMap::new();
        conditional.insert(
            ConditionalExpression::new(")("),
            Target {
                build_dependencies: Some(dependencies),
                ..Target::default()
            },
        );
        let targets = Targets {
            default_target: None,
            conditional: Some(conditional),
        };

        let result = from_targets_v1_to_conditional_requirements(&targets);
        assert!(
            result.is_err(),
            "a malformed selector expression must return an error, not panic"
        );
    }

    /// Conditional extras must be wrapped in a `Conditional` so the resulting
    /// recipe only pulls them in when the expression holds.
    #[test]
    fn test_conditional_extras_conversion() {
        let mut dependencies = OrderMap::new();
        dependencies.insert(
            SourcePackageName::from(PackageName::new_unchecked("gtest")),
            BinaryPackageSpec {
                version: Some("*".parse().unwrap()),
                ..BinaryPackageSpec::default()
            }
            .into(),
        );

        let mut extras = OrderMap::new();
        extras.insert(
            pixi_build_types::ExtraGroupName::new("test").unwrap(),
            dependencies,
        );

        let mut conditional = OrderMap::new();
        conditional.insert(
            ConditionalExpression::new("win"),
            Target {
                extra_dependencies: Some(extras),
                ..Target::default()
            },
        );
        let targets = Targets {
            default_target: None,
            conditional: Some(conditional),
        };

        let requirements = from_targets_v1_to_conditional_requirements(&targets).unwrap();
        let test_group = requirements
            .extras
            .get("test")
            .expect("test group is present");
        let first = test_group
            .iter()
            .next()
            .expect("group has at least one item");
        assert!(
            matches!(first, Item::Conditional(_)),
            "conditional extras must be wrapped in a Conditional, got: {first:?}",
        );
    }

    /// Regression test for <https://github.com/prefix-dev/pixi/issues/4526>:
    /// `version = "*"` combined with a `build` constraint must preserve both
    /// fields so the resulting `MatchSpec` round-trips correctly through its
    /// `Display`/`FromStr` representation (e.g. `hdf5 * *openmpi*`).
    #[test]
    fn test_binary_package_conversion_any_version_with_build_preserves_version() {
        let name = PackageName::new_unchecked("hdf5");
        let spec = BinaryPackageSpec {
            version: Some("*".parse().unwrap()),
            build: Some("*openmpi*".parse().unwrap()),
            ..BinaryPackageSpec::default()
        };
        let match_spec = binary_package_spec_to_package_dependency(name, spec);
        assert_eq!(match_spec.to_string(), "hdf5 * *openmpi*");
    }

    /// A missing version combined with a `build` constraint should be treated
    /// as `*` so the resulting `MatchSpec` does not promote the build glob to
    /// a version constraint when rendered to a string.
    #[test]
    fn test_binary_package_conversion_no_version_with_build_inserts_any() {
        let name = PackageName::new_unchecked("hdf5");
        let spec = BinaryPackageSpec {
            version: None,
            build: Some("*openmpi*".parse().unwrap()),
            ..BinaryPackageSpec::default()
        };
        let match_spec = binary_package_spec_to_package_dependency(name, spec);
        assert_eq!(match_spec.to_string(), "hdf5 * *openmpi*");
    }

    /// Build a `pbt::Target` whose only populated field is `run_constraints`.
    fn target_with_only_run_constraints(name: &str, version: &str) -> Target {
        let mut constraints = OrderMap::new();
        constraints.insert(
            SourcePackageName::from(PackageName::new_unchecked(name)),
            BinaryPackageSpec {
                version: Some(version.parse().unwrap()),
                ..BinaryPackageSpec::default()
            }
            .into(),
        );
        Target {
            host_dependencies: None,
            build_dependencies: None,
            run_dependencies: None,
            run_constraints: Some(constraints),
            extra_dependencies: None,
            run_exports: None,
        }
    }

    /// Regression test: `From<&Target>` must read `target.run_constraints` and
    /// fill `bin_reqs.run_constraints`. The `PackageSpecDependencies` field
    /// existed before this was wired up; a regression would silently leave it
    /// empty.
    #[test]
    fn test_target_run_constraints_propagate_to_package_spec_dependencies() {
        let target = target_with_only_run_constraints("constrained", ">=1.0");

        let bin_reqs = PackageSpecDependencies::from(&target);

        assert!(bin_reqs.build.is_empty());
        assert!(bin_reqs.host.is_empty());
        assert!(bin_reqs.run.is_empty());
        assert_eq!(bin_reqs.run_constraints.len(), 1);
        let (name, dep) = bin_reqs.run_constraints.iter().next().unwrap();
        assert_eq!(name.as_normalized(), "constrained");
        assert_eq!(dep.to_string(), "constrained >=1.0");
    }

    /// Regression test: `from_targets_v1_to_conditional_requirements` must
    /// populate `Requirements.run_constraints` from both the default target and
    /// conditional targets. The variable was being created and threaded
    /// to the output but never extended.
    #[test]
    fn test_targets_v1_run_constraints_in_requirements() {
        // Default-target run-constraint plus a linux-64 specific one.
        let mut conditional_map = OrderMap::new();
        conditional_map.insert(
            ConditionalExpression::new("host_platform == 'linux-64'"),
            target_with_only_run_constraints("linux-only", ">=2.0"),
        );
        let targets = Targets {
            default_target: Some(target_with_only_run_constraints("everywhere", ">=1.0")),
            conditional: Some(conditional_map),
        };

        let req = from_targets_v1_to_conditional_requirements(&targets).unwrap();
        assert!(req.build.is_empty());
        assert!(req.host.is_empty());
        assert!(req.run.is_empty());
        assert_eq!(
            req.run_constraints.len(),
            2,
            "expected one default and one linux-64 entry"
        );

        let mut items = req.run_constraints.iter();
        // Default target → bare value.
        let default_value = items
            .next()
            .unwrap()
            .as_value()
            .expect("default-target constraint should be a bare value")
            .as_concrete()
            .expect("expected a concrete match spec");
        assert_eq!(default_value.0.to_string(), "everywhere >=1.0");

        // Conditional target → wrapped in a Conditional.
        let conditional = match items.next().unwrap() {
            Item::Conditional(c) => c,
            Item::Value(_) => panic!("expected conditional constraint to be Conditional"),
        };
        let then_item = conditional
            .then
            .iter()
            .next()
            .expect("conditional then-branch must contain the constraint");
        let then_value = then_item
            .as_value()
            .expect("then-branch should hold a value")
            .as_concrete()
            .expect("expected a concrete match spec");
        assert_eq!(then_value.0.to_string(), "linux-only >=2.0");
    }

    fn item_template_str(item: &Item<SerializableMatchSpec>) -> String {
        item.as_value()
            .expect("expected a Value item")
            .as_template()
            .expect("expected a template value")
            .as_str()
            .to_string()
    }

    #[test]
    fn test_pin_subpackage_item_exact() {
        let pin = PinCompatibleSpec {
            lower_bound: None,
            upper_bound: None,
            exact: true,
            build: None,
        };
        let item = pin_subpackage_item("my-package", &pin);
        assert_eq!(
            item_template_str(&item),
            "${{ pin_subpackage('my-package', exact=true) }}"
        );
    }

    #[test]
    fn test_pin_subpackage_item_bounds_and_build() {
        let pin = PinCompatibleSpec {
            lower_bound: Some(PinBound::Expression(pixi_build_types::PinExpression(
                "x.x".to_string(),
            ))),
            upper_bound: Some(PinBound::Expression(pixi_build_types::PinExpression(
                "x.x.x".to_string(),
            ))),
            exact: false,
            build: Some("py*".to_string()),
        };
        let item = pin_subpackage_item("my-package", &pin);
        assert_eq!(
            item_template_str(&item),
            "${{ pin_subpackage('my-package', lower_bound='x.x', upper_bound='x.x.x', build='py*') }}"
        );
    }

    #[test]
    fn test_pin_subpackage_item_literal_version_bound() {
        let pin = PinCompatibleSpec {
            lower_bound: Some(PinBound::Version("1.2.3".parse().unwrap())),
            upper_bound: None,
            exact: false,
            build: None,
        };
        let item = pin_subpackage_item("my-package", &pin);
        assert_eq!(
            item_template_str(&item),
            "${{ pin_subpackage('my-package', lower_bound='1.2.3') }}"
        );
    }

    #[test]
    fn test_pin_subpackage_item_no_bounds_is_bare_pin() {
        let pin = PinCompatibleSpec {
            lower_bound: None,
            upper_bound: None,
            exact: false,
            build: None,
        };
        let item = pin_subpackage_item("my-package", &pin);
        assert_eq!(
            item_template_str(&item),
            "${{ pin_subpackage('my-package') }}"
        );
    }

    /// A `build` matcher containing a single quote must not be able to break
    /// out of the Jinja string literal and corrupt the generated template
    /// (regression test: an earlier version interpolated `build` unescaped,
    /// which could produce malformed Jinja for an ordinary, non-adversarial
    /// build string like `py'3.9` and panic via `JinjaTemplate::new(...).expect(...)`).
    #[test]
    fn test_pin_subpackage_item_escapes_quotes_in_build() {
        let pin = PinCompatibleSpec {
            lower_bound: None,
            upper_bound: None,
            exact: false,
            build: Some("py'3.9".to_string()),
        };
        let item = pin_subpackage_item("my-package", &pin);
        assert_eq!(
            item_template_str(&item),
            r"${{ pin_subpackage('my-package', build='py\'3.9') }}"
        );
    }

    /// A package name containing a single quote (defense in depth; package
    /// names are validated elsewhere and shouldn't actually contain one, but
    /// `pin_subpackage_item` itself must not assume that) must also be
    /// escaped rather than corrupting the generated template.
    #[test]
    fn test_pin_subpackage_item_escapes_quotes_in_name() {
        let pin = PinCompatibleSpec {
            lower_bound: None,
            upper_bound: None,
            exact: true,
            build: None,
        };
        let item = pin_subpackage_item("weird'name", &pin);
        assert_eq!(
            item_template_str(&item),
            r"${{ pin_subpackage('weird\'name', exact=true) }}"
        );
    }

    /// A `PackageSpec::PinCompatible` entry in a regular dependency table
    /// (not just run-exports) must render as a `pin_subpackage(...)` Jinja
    /// item rather than erroring.
    #[test]
    fn test_pin_compatible_in_regular_dependency_table() {
        let mut dependencies = OrderMap::new();
        dependencies.insert(
            SourcePackageName::from(PackageName::new_unchecked("my-package")),
            PackageSpec::PinCompatible(PinCompatibleSpec {
                lower_bound: None,
                upper_bound: None,
                exact: true,
                build: None,
            }),
        );

        let targets = Targets {
            default_target: Some(Target {
                run_dependencies: Some(dependencies),
                ..Target::default()
            }),
            conditional: None,
        };

        let requirements = from_targets_v1_to_conditional_requirements(&targets).unwrap();
        assert_eq!(requirements.run.len(), 1);
        let item = requirements.run.iter().next().unwrap();
        assert_eq!(
            item_template_str(item),
            "${{ pin_subpackage('my-package', exact=true) }}"
        );
    }

    /// `[package.run-exports.*]` buckets must be wired into
    /// `Requirements.run_exports`, with `pin-subpackage` entries rendered as
    /// Jinja items just like in the regular dependency tables.
    #[test]
    fn test_run_exports_wired_into_requirements() {
        let mut weak = OrderMap::new();
        weak.insert(
            SourcePackageName::from(PackageName::new_unchecked("my-package")),
            PackageSpec::PinCompatible(PinCompatibleSpec {
                lower_bound: None,
                upper_bound: None,
                exact: true,
                build: None,
            }),
        );
        let mut strong = OrderMap::new();
        strong.insert(
            SourcePackageName::from(PackageName::new_unchecked("libzma")),
            BinaryPackageSpec {
                version: Some("*".parse().unwrap()),
                ..BinaryPackageSpec::default()
            }
            .into(),
        );

        let targets = Targets {
            default_target: Some(Target {
                run_exports: Some(pixi_build_types::RunExportsTarget {
                    weak: Some(weak),
                    strong: Some(strong),
                    ..Default::default()
                }),
                ..Target::default()
            }),
            conditional: None,
        };

        let requirements = from_targets_v1_to_conditional_requirements(&targets).unwrap();
        assert_eq!(requirements.run_exports.weak.len(), 1);
        let weak_item = requirements.run_exports.weak.iter().next().unwrap();
        assert_eq!(
            item_template_str(weak_item),
            "${{ pin_subpackage('my-package', exact=true) }}"
        );

        assert_eq!(requirements.run_exports.strong.len(), 1);
        let strong_item = requirements.run_exports.strong.iter().next().unwrap();
        let strong_value = strong_item
            .as_value()
            .expect("expected a Value item")
            .as_concrete()
            .expect("expected a concrete match spec");
        assert_eq!(strong_value.0.to_string(), "libzma");

        // Buckets that weren't populated stay empty.
        assert!(requirements.run_exports.noarch.is_empty());
        assert!(requirements.run_exports.strong_constraints.is_empty());
        assert!(requirements.run_exports.weak_constraints.is_empty());
    }

    #[test]
    fn test_targets_v1_run_exports_with_source_dependency() {
        use pixi_build_types::PathSpec;

        let mut weak = OrderMap::new();
        weak.insert(
            SourcePackageName::from(PackageName::new_unchecked("sibling-package")),
            PackageSpec::Source(SourcePackageSpec::from(PathSpec {
                path: "../sibling-package".to_string(),
            })),
        );

        let targets = Targets {
            default_target: Some(Target {
                run_exports: Some(pixi_build_types::RunExportsTarget {
                    weak: Some(weak),
                    ..Default::default()
                }),
                ..Target::default()
            }),
            conditional: None,
        };

        let requirements = from_targets_v1_to_conditional_requirements(&targets).unwrap();
        let weak_items: Vec<_> = requirements.run_exports.weak.iter().collect();
        assert_eq!(
            weak_items.len(),
            1,
            "default-target source run-export should produce exactly one item"
        );
        let concrete = weak_items[0]
            .as_value()
            .expect("default-target run-export should be a bare value")
            .as_concrete()
            .expect("expected a concrete match spec");
        let match_spec = &concrete.0;
        assert_eq!(
            match_spec
                .name
                .as_exact()
                .expect("name should be exact")
                .as_normalized(),
            "sibling-package",
        );
        let url = match_spec
            .url
            .as_ref()
            .expect("source run-export must serialize with a source URL");
        assert_eq!(url.scheme(), "source");
        let path_pair = url
            .query_pairs()
            .find(|(k, _)| k == "path")
            .expect("source URL must encode the path");
        assert_eq!(path_pair.1, "../sibling-package");

        // Other kinds must remain empty.
        assert_eq!(requirements.run_exports.strong.iter().count(), 0);
        assert!(requirements.run_exports.noarch.is_empty());
        assert!(requirements.run_exports.strong_constraints.is_empty());
        assert!(requirements.run_exports.weak_constraints.is_empty());
    }
}
