//! Deterministic out-of-fold module matrix construction.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use dataset::{OuterFold, Sample};
use domain::UnixNanos;
use feature_registry::{FeatureEntity, FeatureId, FiniteF64, MissingnessReason, QualityScore};

use crate::{
    CuspEligibilityReceipt, EnsembleError, MatrixConstraints, ModuleAvailability, ModuleDatum,
    ModuleKind, ModuleOutput, hash_i64, hash_string, hash_u64,
    module_output::{
        entity_key, hash_availability, hash_entity, hash_module_datum, missingness_identity_tag,
    },
};

const ENSEMBLE_SCHEMA_VERSION: u32 = 1;
const MAXIMUM_ROWS: usize = 1_000_000;
const MAXIMUM_COLUMNS: usize = 256;
const MAXIMUM_MODULES: usize = ModuleKind::ALL.len();
const MAXIMUM_DENSE_CELLS: usize = 4_000_000;
const SCHEMA_DOMAIN: &[u8] = b"cmti:ensemble-schema:v1\0";
const MATRIX_DOMAIN: &[u8] = b"cmti:out-of-fold-module-matrix:v2\0";

/// One predeclared scalar column and its owning independent module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatrixColumn {
    module_kind: ModuleKind,
    feature_id: FeatureId,
}

impl MatrixColumn {
    #[must_use]
    pub const fn new(module_kind: ModuleKind, feature_id: FeatureId) -> Self {
        Self {
            module_kind,
            feature_id,
        }
    }

    #[must_use]
    pub fn id(&self) -> &str {
        self.feature_id.as_str()
    }

    #[must_use]
    pub const fn module_kind(&self) -> ModuleKind {
        self.module_kind
    }

    #[must_use]
    pub const fn feature_id(&self) -> &FeatureId {
        &self.feature_id
    }
}

/// Immutable, schema-first column contract for one ensemble matrix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnsembleSchema {
    version: u32,
    columns: Vec<MatrixColumn>,
    module_features: BTreeMap<ModuleKind, Vec<FeatureId>>,
    evidence_hash: [u8; 32],
}

impl EnsembleSchema {
    pub fn try_new(version: u32, mut columns: Vec<MatrixColumn>) -> Result<Self, EnsembleError> {
        if version != ENSEMBLE_SCHEMA_VERSION {
            return Err(EnsembleError::UnsupportedSchema);
        }
        if columns.is_empty() {
            return Err(EnsembleError::EmptyColumns);
        }
        if columns.len() > MAXIMUM_COLUMNS {
            return Err(EnsembleError::ColumnCapacity);
        }
        columns.sort_by(|left, right| {
            (left.module_kind, left.feature_id.as_str())
                .cmp(&(right.module_kind, right.feature_id.as_str()))
        });
        let mut global_ids = BTreeSet::new();
        let mut module_features = BTreeMap::<ModuleKind, Vec<FeatureId>>::new();
        for column in &columns {
            if !global_ids.insert(column.feature_id.clone()) {
                return Err(EnsembleError::DuplicateColumn);
            }
            module_features
                .entry(column.module_kind)
                .or_default()
                .push(column.feature_id.clone());
        }
        if module_features.len() > MAXIMUM_MODULES {
            return Err(EnsembleError::ColumnCapacity);
        }
        let evidence_hash = calculate_schema_hash(version, &columns);
        Ok(Self {
            version,
            columns,
            module_features,
            evidence_hash,
        })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub fn columns(&self) -> &[MatrixColumn] {
        &self.columns
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// Bounded work limits, lowerable for a specific build but never expandable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MatrixLimits {
    max_rows: usize,
    max_columns: usize,
    max_cells: usize,
}

impl MatrixLimits {
    pub fn try_new(
        max_rows: usize,
        max_columns: usize,
        max_cells: usize,
    ) -> Result<Self, EnsembleError> {
        if max_rows == 0
            || max_columns == 0
            || max_cells == 0
            || max_rows > MAXIMUM_ROWS
            || max_columns > MAXIMUM_COLUMNS
            || max_cells > MAXIMUM_DENSE_CELLS
        {
            return Err(EnsembleError::InvalidLimits);
        }
        Ok(Self {
            max_rows,
            max_columns,
            max_cells,
        })
    }

    #[must_use]
    pub const fn production() -> Self {
        Self {
            max_rows: MAXIMUM_ROWS,
            max_columns: MAXIMUM_COLUMNS,
            max_cells: MAXIMUM_DENSE_CELLS,
        }
    }
}

impl Default for MatrixLimits {
    fn default() -> Self {
        Self::production()
    }
}

/// Explicit state for one schema-declared module in one row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RowModule {
    Output(Box<ModuleOutput>),
    Missing {
        module_kind: ModuleKind,
        availability: ModuleAvailability,
    },
}

impl RowModule {
    #[must_use]
    pub fn output(output: ModuleOutput) -> Self {
        Self::Output(Box::new(output))
    }

    #[must_use]
    pub const fn module_kind(&self) -> ModuleKind {
        match self {
            Self::Output(output) => output.module_kind(),
            Self::Missing { module_kind, .. } => *module_kind,
        }
    }
}

/// One untouched outer-fold sample with an explicit state for every module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrainingRowInput {
    pub sample: Sample,
    pub entity: FeatureEntity,
    pub modules: Vec<RowModule>,
}

/// Matrix-visible distinction between reported and explicitly absent modules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatrixModuleState {
    Reported(ModuleAvailability),
    Missing(ModuleAvailability),
}

impl MatrixModuleState {
    const fn identity_tag(self) -> u8 {
        match self {
            Self::Reported(_) => 1,
            Self::Missing(_) => 2,
        }
    }

    const fn availability(self) -> ModuleAvailability {
        match self {
            Self::Reported(availability) | Self::Missing(availability) => availability,
        }
    }
}

/// Exact value state retained by the matrix without zero imputation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatrixDatum {
    Present(FiniteF64),
    Missing(MissingnessReason),
    ModuleMissing(MissingnessReason),
}

/// One canonical row accepted by the later stacker training stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatrixRow {
    sample: Sample,
    entity: FeatureEntity,
    values: Vec<MatrixDatum>,
    module_states: BTreeMap<ModuleKind, MatrixModuleState>,
    module_quality: BTreeMap<ModuleKind, QualityScore>,
    output_hashes: BTreeMap<ModuleKind, [u8; 32]>,
    column_ids: Arc<[String]>,
}

impl MatrixRow {
    #[must_use]
    pub const fn row_id(&self) -> u64 {
        self.sample.id()
    }

    #[must_use]
    pub const fn entity(&self) -> &FeatureEntity {
        &self.entity
    }

    #[must_use]
    pub const fn origin(&self) -> UnixNanos {
        UnixNanos::new(self.sample.origin_time_ns())
    }

    #[must_use]
    pub const fn outcome_end(&self) -> UnixNanos {
        UnixNanos::new(self.sample.outcome_end_ns())
    }

    #[must_use]
    pub const fn outcome_known_at(&self) -> UnixNanos {
        UnixNanos::new(self.sample.as_known_at_ns())
    }

    #[must_use]
    pub fn values(&self) -> &[MatrixDatum] {
        &self.values
    }

    #[must_use]
    pub fn value(&self, column_id: &str) -> Option<&MatrixDatum> {
        self.column_ids
            .iter()
            .position(|candidate| candidate == column_id)
            .map(|index| &self.values[index])
    }

    #[must_use]
    pub fn module_state(&self, module_kind: ModuleKind) -> Option<MatrixModuleState> {
        self.module_states.get(&module_kind).copied()
    }

    #[must_use]
    pub fn module_quality(&self, module_kind: ModuleKind) -> Option<QualityScore> {
        self.module_quality.get(&module_kind).copied()
    }
}

/// Canonical bounded matrix accepted by the later stacker training stage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutOfFoldMatrix {
    schema: EnsembleSchema,
    rows: Vec<MatrixRow>,
    constraints: MatrixConstraints,
    outer_fold_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl OutOfFoldMatrix {
    pub fn build(
        schema: &EnsembleSchema,
        outer_fold: &OuterFold,
        inputs: Vec<TrainingRowInput>,
        cusp_receipt: CuspEligibilityReceipt,
    ) -> Result<Self, EnsembleError> {
        Self::build_with_limits(
            schema,
            outer_fold,
            inputs,
            cusp_receipt,
            MatrixLimits::production(),
        )
    }

    pub fn build_with_limits(
        schema: &EnsembleSchema,
        outer_fold: &OuterFold,
        inputs: Vec<TrainingRowInput>,
        cusp_receipt: CuspEligibilityReceipt,
        limits: MatrixLimits,
    ) -> Result<Self, EnsembleError> {
        validate_limits(limits)?;
        if inputs.is_empty() {
            return Err(EnsembleError::EmptyRows);
        }
        if inputs.len() > limits.max_rows {
            return Err(EnsembleError::RowCapacity);
        }
        if schema.columns.len() > limits.max_columns {
            return Err(EnsembleError::ColumnCapacity);
        }
        preflight_dense_cells(inputs.len(), schema.columns.len(), limits.max_cells)?;

        let mut keyed_inputs = inputs
            .into_iter()
            .map(|input| Ok((entity_key(&input.entity)?, input)))
            .collect::<Result<Vec<_>, EnsembleError>>()?;
        keyed_inputs.sort_by(|(left_entity, left), (right_entity, right)| {
            (
                left.sample.origin_time_ns(),
                left.sample.id(),
                left_entity.as_slice(),
            )
                .cmp(&(
                    right.sample.origin_time_ns(),
                    right.sample.id(),
                    right_entity.as_slice(),
                ))
        });

        let constraints = MatrixConstraints::new(&schema.columns, cusp_receipt);
        let column_ids: Arc<[String]> = schema
            .columns
            .iter()
            .map(|column| column.id().to_owned())
            .collect::<Vec<_>>()
            .into();
        let rows = validate_and_build_rows(
            schema,
            outer_fold,
            keyed_inputs,
            Arc::clone(&column_ids),
            cusp_receipt,
        )?;
        let evidence_hash = calculate_matrix_hash(schema, outer_fold, &rows, &constraints)?;
        Ok(Self {
            schema: schema.clone(),
            rows,
            constraints,
            outer_fold_hash: outer_fold.fold_hash(),
            evidence_hash,
        })
    }

    #[must_use]
    pub const fn schema(&self) -> &EnsembleSchema {
        &self.schema
    }

    #[must_use]
    pub fn columns(&self) -> &[MatrixColumn] {
        self.schema.columns()
    }

    #[must_use]
    pub fn rows(&self) -> &[MatrixRow] {
        &self.rows
    }

    #[must_use]
    pub const fn constraints(&self) -> &MatrixConstraints {
        &self.constraints
    }

    #[must_use]
    pub const fn outer_fold_hash(&self) -> [u8; 32] {
        self.outer_fold_hash
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

fn validate_limits(limits: MatrixLimits) -> Result<(), EnsembleError> {
    MatrixLimits::try_new(limits.max_rows, limits.max_columns, limits.max_cells).map(|_| ())
}

fn preflight_dense_cells(
    row_count: usize,
    column_count: usize,
    max_cells: usize,
) -> Result<(), EnsembleError> {
    let cells = row_count
        .checked_mul(column_count)
        .ok_or(EnsembleError::CellCapacity)?;
    if cells > max_cells {
        Err(EnsembleError::CellCapacity)
    } else {
        Ok(())
    }
}

fn validate_and_build_rows(
    schema: &EnsembleSchema,
    outer_fold: &OuterFold,
    keyed_inputs: Vec<(Vec<u8>, TrainingRowInput)>,
    column_ids: Arc<[String]>,
    cusp_receipt: CuspEligibilityReceipt,
) -> Result<Vec<MatrixRow>, EnsembleError> {
    let mut row_ids = BTreeSet::new();
    let mut row_origins = BTreeSet::new();
    let mut module_identities = BTreeMap::<ModuleKind, (String, [u8; 32])>::new();
    let mut rows = Vec::with_capacity(keyed_inputs.len());
    let test = outer_fold.test();

    for (entity_key, input) in keyed_inputs {
        if !row_ids.insert(input.sample.id())
            || !row_origins.insert((entity_key, input.sample.origin_time_ns()))
        {
            return Err(EnsembleError::DuplicateRow);
        }
        if input.sample.origin_time_ns() < test.start_ns()
            || input.sample.origin_time_ns() >= test.end_ns()
            || input.sample.outcome_end_ns() > test.end_ns()
            || input.sample.as_known_at_ns() > test.end_ns()
        {
            return Err(EnsembleError::InvalidRow);
        }
        let modules = validate_row_modules(schema, outer_fold, &input)?;
        register_module_identities(&modules, &mut module_identities)?;
        rows.push(build_row(schema, input, modules, Arc::clone(&column_ids))?);
    }
    validate_cusp_candidate(schema, cusp_receipt, &module_identities)?;
    Ok(rows)
}

fn validate_row_modules(
    schema: &EnsembleSchema,
    outer_fold: &OuterFold,
    input: &TrainingRowInput,
) -> Result<BTreeMap<ModuleKind, RowModule>, EnsembleError> {
    let mut modules = BTreeMap::new();
    for module in &input.modules {
        let module_kind = module.module_kind();
        if modules.insert(module_kind, module.clone()).is_some() {
            return Err(EnsembleError::DuplicateModule);
        }
    }
    if modules.keys().ne(schema.module_features.keys()) {
        return Err(EnsembleError::ModuleCoverageMismatch);
    }

    for (module_kind, module) in &modules {
        let expected_features = &schema.module_features[module_kind];
        match module {
            RowModule::Output(output) => {
                if output.outer_fold_hash() != outer_fold.fold_hash() {
                    return Err(EnsembleError::FoldMismatch);
                }
                if output.entity() != &input.entity {
                    return Err(EnsembleError::EntityMismatch);
                }
                if output.origin().value() != input.sample.origin_time_ns() {
                    return Err(EnsembleError::OriginMismatch);
                }
                if output.as_known_at().value() > input.sample.origin_time_ns() {
                    return Err(EnsembleError::FutureKnownOutput);
                }
                if output.trained_through().value() >= outer_fold.test().start_ns() {
                    return Err(EnsembleError::InFoldPrediction);
                }
                if output.values().keys().ne(expected_features.iter()) {
                    return Err(EnsembleError::SchemaMismatch);
                }
            }
            RowModule::Missing { availability, .. } => {
                if !availability.permits_absent_output() {
                    return Err(EnsembleError::InvalidMissingModule);
                }
            }
        }
    }
    Ok(modules)
}

fn register_module_identities(
    modules: &BTreeMap<ModuleKind, RowModule>,
    identities: &mut BTreeMap<ModuleKind, (String, [u8; 32])>,
) -> Result<(), EnsembleError> {
    for (module_kind, module) in modules {
        let RowModule::Output(output) = module else {
            continue;
        };
        let identity = (output.model_id().to_owned(), output.model_package_hash());
        if let Some(expected) = identities.get(module_kind) {
            if expected != &identity {
                return Err(EnsembleError::ModulePackageMismatch);
            }
        } else {
            identities.insert(*module_kind, identity);
        }
    }
    Ok(())
}

fn validate_cusp_candidate(
    schema: &EnsembleSchema,
    receipt: CuspEligibilityReceipt,
    identities: &BTreeMap<ModuleKind, (String, [u8; 32])>,
) -> Result<(), EnsembleError> {
    if !receipt.is_eligible() || !schema.module_features.contains_key(&ModuleKind::Cusp) {
        return Ok(());
    }
    match identities.get(&ModuleKind::Cusp) {
        Some((_, package_hash)) if *package_hash == receipt.candidate_model_hash() => Ok(()),
        Some(_) | None => Err(EnsembleError::CuspCandidateMismatch),
    }
}

fn build_row(
    schema: &EnsembleSchema,
    input: TrainingRowInput,
    modules: BTreeMap<ModuleKind, RowModule>,
    column_ids: Arc<[String]>,
) -> Result<MatrixRow, EnsembleError> {
    let mut module_states = BTreeMap::new();
    let mut module_quality = BTreeMap::new();
    let mut output_hashes = BTreeMap::new();
    for (module_kind, module) in &modules {
        match module {
            RowModule::Output(output) => {
                module_states.insert(
                    *module_kind,
                    MatrixModuleState::Reported(output.availability()),
                );
                module_quality.insert(*module_kind, output.quality());
                output_hashes.insert(*module_kind, output.output_hash());
            }
            RowModule::Missing { availability, .. } => {
                module_states.insert(*module_kind, MatrixModuleState::Missing(*availability));
            }
        }
    }
    let values = schema
        .columns
        .iter()
        .map(|column| {
            let datum = match &modules[&column.module_kind] {
                RowModule::Output(output) => match &output.values()[&column.feature_id] {
                    ModuleDatum::Present(value) => MatrixDatum::Present(*value),
                    ModuleDatum::Missing(reason) => MatrixDatum::Missing(*reason),
                },
                RowModule::Missing { availability, .. } => availability
                    .reason()
                    .map(MatrixDatum::ModuleMissing)
                    .ok_or(EnsembleError::InvalidMissingModule)?,
            };
            Ok(datum)
        })
        .collect::<Result<Vec<_>, EnsembleError>>()?;
    Ok(MatrixRow {
        sample: input.sample,
        entity: input.entity,
        values,
        module_states,
        module_quality,
        output_hashes,
        column_ids,
    })
}

fn calculate_schema_hash(version: u32, columns: &[MatrixColumn]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SCHEMA_DOMAIN);
    hasher.update(&version.to_le_bytes());
    hash_u64(
        &mut hasher,
        u64::try_from(columns.len()).unwrap_or(u64::MAX),
    );
    for column in columns {
        hasher.update(&[column.module_kind.identity_tag()]);
        hash_string(&mut hasher, column.feature_id.as_str());
        hasher.update(&[1, 1]); // finite f64 and explicit reasoned missingness
    }
    *hasher.finalize().as_bytes()
}

fn calculate_matrix_hash(
    schema: &EnsembleSchema,
    outer_fold: &OuterFold,
    rows: &[MatrixRow],
    constraints: &MatrixConstraints,
) -> Result<[u8; 32], EnsembleError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MATRIX_DOMAIN);
    hasher.update(&schema.evidence_hash);
    hasher.update(&constraints.evidence_hash());
    hasher.update(&outer_fold.fold_hash());
    hash_i64(&mut hasher, outer_fold.test().start_ns());
    hash_i64(&mut hasher, outer_fold.test().end_ns());
    hash_u64(&mut hasher, u64::try_from(rows.len()).unwrap_or(u64::MAX));
    for row in rows {
        hash_u64(&mut hasher, row.sample.id());
        hash_i64(&mut hasher, row.sample.origin_time_ns());
        hash_i64(&mut hasher, row.sample.outcome_end_ns());
        hash_i64(&mut hasher, row.sample.as_known_at_ns());
        hash_entity(&mut hasher, &row.entity)?;
        for module_kind in schema.module_features.keys() {
            let state = row.module_states[module_kind];
            hasher.update(&[module_kind.identity_tag(), state.identity_tag()]);
            hash_availability(&mut hasher, state.availability());
            if let Some(quality) = row.module_quality.get(module_kind) {
                hasher.update(&[1]);
                hash_u64(&mut hasher, u64::from(quality.millionths()));
            } else {
                hasher.update(&[0]);
            }
            if let Some(output_hash) = row.output_hashes.get(module_kind) {
                hasher.update(&[1]);
                hasher.update(output_hash);
            } else {
                hasher.update(&[0]);
            }
        }
        for value in &row.values {
            match value {
                MatrixDatum::Present(value) => {
                    hash_module_datum(&mut hasher, &ModuleDatum::Present(*value));
                }
                MatrixDatum::Missing(reason) => {
                    hash_module_datum(&mut hasher, &ModuleDatum::Missing(*reason));
                }
                MatrixDatum::ModuleMissing(reason) => {
                    hasher.update(&[3, missingness_identity_tag(*reason)]);
                }
            }
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{EnsembleError, preflight_dense_cells};

    #[test]
    fn dense_capacity_preflight_rejects_overflow_before_allocation() {
        assert_eq!(
            preflight_dense_cells(usize::MAX, 2, usize::MAX),
            Err(EnsembleError::CellCapacity)
        );
    }
}
