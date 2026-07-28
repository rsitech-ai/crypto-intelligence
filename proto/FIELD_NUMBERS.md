# CMTI Protobuf Field Number Registry

This file is generated from the reviewed descriptor by `cargo run -p xtask -- generate-field-registry`. Deleted field and enum names and numbers remain in the reserved tables and must never be reused.

## Message fields

| Fully qualified message | Field | Number | Status |
|---|---|---:|---|
| `cmti.admin.v1.ApplyConfigurationRequest` | `actor` | 3 | Active |
| `cmti.admin.v1.ApplyConfigurationRequest` | `expected_configuration_blake3` | 1 | Active |
| `cmti.admin.v1.ApplyConfigurationRequest` | `proposed_configuration` | 2 | Active |
| `cmti.admin.v1.ApplyConfigurationResponse` | `new_configuration_blake3` | 2 | Active |
| `cmti.admin.v1.ApplyConfigurationResponse` | `old_configuration_blake3` | 1 | Active |
| `cmti.admin.v1.ApplyConfigurationResponse` | `restart_required` | 3 | Active |
| `cmti.admin.v1.GetCapabilitiesResponse` | `available` | 3 | Active |
| `cmti.admin.v1.GetCapabilitiesResponse` | `capabilities` | 1 | Active |
| `cmti.admin.v1.GetCapabilitiesResponse` | `maximum_batch_size` | 2 | Active |
| `cmti.admin.v1.GetModelCardRequest` | `model_package_id` | 1 | Active |
| `cmti.admin.v1.GetModelCardResponse` | `artifact_blake3` | 3 | Active |
| `cmti.admin.v1.GetModelCardResponse` | `model_card_json` | 2 | Active |
| `cmti.admin.v1.GetModelCardResponse` | `model_package_id` | 1 | Active |
| `cmti.admin.v1.GetRuntimeStatusResponse` | `configuration_blake3` | 3 | Active |
| `cmti.admin.v1.GetRuntimeStatusResponse` | `observed_at` | 2 | Active |
| `cmti.admin.v1.GetRuntimeStatusResponse` | `runtime_state` | 1 | Active |
| `cmti.admin.v1.GetRuntimeStatusResponse` | `uptime_seconds` | 4 | Active |
| `cmti.admin.v1.InferBatchRequest` | `deadline_millis` | 3 | Active |
| `cmti.admin.v1.InferBatchRequest` | `inputs` | 2 | Active |
| `cmti.admin.v1.InferBatchRequest` | `model_package_id` | 1 | Active |
| `cmti.admin.v1.InferBatchResponse` | `elapsed_micros` | 2 | Active |
| `cmti.admin.v1.InferBatchResponse` | `outputs` | 1 | Active |
| `cmti.admin.v1.ListModelsResponse` | `model_package_ids` | 1 | Active |
| `cmti.admin.v1.RequestGracefulShutdownRequest` | `reason` | 1 | Active |
| `cmti.admin.v1.RequestGracefulShutdownResponse` | `accepted` | 1 | Active |
| `cmti.admin.v1.RequestGracefulShutdownResponse` | `deadline` | 2 | Active |
| `cmti.admin.v1.WarmModelRequest` | `model_package_id` | 1 | Active |
| `cmti.admin.v1.WarmModelResponse` | `ready` | 1 | Active |
| `cmti.admin.v1.WarmModelResponse` | `resident_bytes` | 2 | Active |
| `cmti.common.v1.AssetId` | `canonical_symbol` | 4 | Active |
| `cmti.common.v1.AssetId` | `chain_id` | 2 | Active |
| `cmti.common.v1.AssetId` | `contract_or_mint` | 3 | Active |
| `cmti.common.v1.AssetId` | `generation` | 5 | Active |
| `cmti.common.v1.AssetId` | `namespace` | 1 | Active |
| `cmti.common.v1.DataQualitySummary` | `degradation_reasons` | 3 | Active |
| `cmti.common.v1.DataQualitySummary` | `quality_flags` | 2 | Active |
| `cmti.common.v1.DataQualitySummary` | `quality_score_ppm` | 1 | Active |
| `cmti.common.v1.ErrorDetail` | `code` | 1 | Active |
| `cmti.common.v1.ErrorDetail` | `reason` | 2 | Active |
| `cmti.common.v1.ErrorDetail` | `retryable` | 3 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `api_major` | 1 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `api_minor` | 2 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `app_build_id` | 3 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `client_nonce` | 7 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `feature_capabilities` | 5 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `model_runtime_capabilities` | 6 | Active |
| `cmti.common.v1.NegotiateSessionRequest` | `schema_bundle_hash` | 4 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `api_major` | 1 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `api_minor` | 2 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `daemon_build_id` | 3 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `expires_at` | 10 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `feature_capabilities` | 5 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `minimum_compatible_version` | 7 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `model_runtime_capabilities` | 6 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `schema_bundle_hash` | 4 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `server_nonce` | 8 | Active |
| `cmti.common.v1.NegotiateSessionResponse` | `session_id` | 9 | Active |
| `cmti.common.v1.ProtocolVersion` | `major` | 1 | Active |
| `cmti.common.v1.ProtocolVersion` | `minor` | 2 | Active |
| `cmti.common.v1.StreamMetadata` | `as_of_time` | 4 | Active |
| `cmti.common.v1.StreamMetadata` | `coalesced_since_previous` | 8 | Active |
| `cmti.common.v1.StreamMetadata` | `delivery_policy` | 9 | Active |
| `cmti.common.v1.StreamMetadata` | `dropped_since_previous` | 7 | Active |
| `cmti.common.v1.StreamMetadata` | `resume_token` | 5 | Active |
| `cmti.common.v1.StreamMetadata` | `schema_version` | 6 | Active |
| `cmti.common.v1.StreamMetadata` | `snapshot_or_delta` | 3 | Active |
| `cmti.common.v1.StreamMetadata` | `stream_id` | 1 | Active |
| `cmti.common.v1.StreamMetadata` | `stream_sequence` | 2 | Active |
| `cmti.common.v1.StreamMetadata` | `terminal_error` | 11 | Active |
| `cmti.common.v1.StreamMetadata` | `terminal_status` | 10 | Active |
| `cmti.common.v1.UnixNanos` | `value` | 1 | Active |
| `cmti.health.v1.CheckResponse` | `protocol` | 2 | Active |
| `cmti.health.v1.CheckResponse` | `status` | 1 | Active |
| `cmti.health.v1.SubscribeQualityRequest` | `resume_token` | 2 | Active |
| `cmti.health.v1.SubscribeQualityRequest` | `source_ids` | 1 | Active |
| `cmti.health.v1.SubscribeQualityResponse` | `quality` | 3 | Active |
| `cmti.health.v1.SubscribeQualityResponse` | `source_id` | 2 | Active |
| `cmti.health.v1.SubscribeQualityResponse` | `stream` | 1 | Active |
| `cmti.market.v1.GetInstrumentRequest` | `generation` | 2 | Active |
| `cmti.market.v1.GetInstrumentRequest` | `instrument_id` | 1 | Active |
| `cmti.market.v1.GetInstrumentResponse` | `base_asset` | 3 | Active |
| `cmti.market.v1.GetInstrumentResponse` | `generation` | 2 | Active |
| `cmti.market.v1.GetInstrumentResponse` | `instrument_id` | 1 | Active |
| `cmti.market.v1.GetInstrumentResponse` | `quote_asset` | 4 | Active |
| `cmti.market.v1.GetInstrumentResponse` | `venue_id` | 5 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `best_ask` | 6 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `best_bid` | 5 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `event_unix_nanos` | 8 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `freshness_millis` | 10 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `generation` | 3 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `health` | 7 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `receive_unix_nanos` | 9 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `sequence` | 4 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `source` | 1 | Active |
| `cmti.market.v1.GetOrderBookSnapshotResponse` | `symbol` | 2 | Active |
| `cmti.market.v1.ListAssetsResponse` | `assets` | 1 | Active |
| `cmti.market.v1.ListVenuesResponse` | `venue_ids` | 1 | Active |
| `cmti.market.v1.SubscribeAssetStateRequest` | `assets` | 1 | Active |
| `cmti.market.v1.SubscribeAssetStateRequest` | `resume_token` | 2 | Active |
| `cmti.market.v1.SubscribeAssetStateResponse` | `asset` | 2 | Active |
| `cmti.market.v1.SubscribeAssetStateResponse` | `consolidated_price` | 3 | Active |
| `cmti.market.v1.SubscribeAssetStateResponse` | `health` | 4 | Active |
| `cmti.market.v1.SubscribeAssetStateResponse` | `stream` | 1 | Active |
| `cmti.market.v1.SubscribeVenueStateRequest` | `resume_token` | 2 | Active |
| `cmti.market.v1.SubscribeVenueStateRequest` | `venue_ids` | 1 | Active |
| `cmti.market.v1.SubscribeVenueStateResponse` | `health` | 3 | Active |
| `cmti.market.v1.SubscribeVenueStateResponse` | `source_latency_millis` | 4 | Active |
| `cmti.market.v1.SubscribeVenueStateResponse` | `stream` | 1 | Active |
| `cmti.market.v1.SubscribeVenueStateResponse` | `venue_id` | 2 | Active |
| `cmti.replay.v1.ControlReplayRequest` | `action` | 2 | Active |
| `cmti.replay.v1.ControlReplayRequest` | `replay_id` | 1 | Active |
| `cmti.replay.v1.ControlReplayRequest` | `seek_cursor` | 3 | Active |
| `cmti.replay.v1.ControlReplayResponse` | `cursor` | 3 | Active |
| `cmti.replay.v1.ControlReplayResponse` | `replay_id` | 1 | Active |
| `cmti.replay.v1.ControlReplayResponse` | `state` | 2 | Active |
| `cmti.replay.v1.CreateReplayRequest` | `instruments` | 2 | Active |
| `cmti.replay.v1.CreateReplayRequest` | `interval` | 1 | Active |
| `cmti.replay.v1.CreateReplayRequest` | `speed_ppm` | 3 | Active |
| `cmti.replay.v1.CreateReplayResponse` | `configuration_blake3` | 4 | Active |
| `cmti.replay.v1.CreateReplayResponse` | `cursor` | 3 | Active |
| `cmti.replay.v1.CreateReplayResponse` | `model_set_blake3` | 5 | Active |
| `cmti.replay.v1.CreateReplayResponse` | `replay_id` | 1 | Active |
| `cmti.replay.v1.CreateReplayResponse` | `state` | 2 | Active |
| `cmti.replay.v1.ReplayInterval` | `end_exclusive` | 2 | Active |
| `cmti.replay.v1.ReplayInterval` | `start_inclusive` | 1 | Active |
| `cmti.replay.v1.SubscribeReplayStateRequest` | `replay_id` | 1 | Active |
| `cmti.replay.v1.SubscribeReplayStateRequest` | `resume_token` | 2 | Active |
| `cmti.replay.v1.SubscribeReplayStateResponse` | `cursor` | 4 | Active |
| `cmti.replay.v1.SubscribeReplayStateResponse` | `replay_id` | 2 | Active |
| `cmti.replay.v1.SubscribeReplayStateResponse` | `state` | 3 | Active |
| `cmti.replay.v1.SubscribeReplayStateResponse` | `stream` | 1 | Active |
| `cmti.risk.v1.AlertEvent` | `alert_event_id` | 1 | Active |
| `cmti.risk.v1.AlertEvent` | `forecast_id` | 4 | Active |
| `cmti.risk.v1.AlertEvent` | `rule_id` | 2 | Active |
| `cmti.risk.v1.AlertEvent` | `triggered_at` | 3 | Active |
| `cmti.risk.v1.AlertRule` | `enabled` | 3 | Active |
| `cmti.risk.v1.AlertRule` | `expression` | 2 | Active |
| `cmti.risk.v1.AlertRule` | `revision` | 4 | Active |
| `cmti.risk.v1.AlertRule` | `rule_id` | 1 | Active |
| `cmti.risk.v1.CuspServiceGetCuspStateRequest` | `asset` | 1 | Active |
| `cmti.risk.v1.CuspServiceGetCuspStateResponse` | `cusp_state` | 1 | Active |
| `cmti.risk.v1.CuspState` | `alpha` | 2 | Active |
| `cmti.risk.v1.CuspState` | `availability` | 5 | Active |
| `cmti.risk.v1.CuspState` | `beta` | 3 | Active |
| `cmti.risk.v1.CuspState` | `fold_distance` | 4 | Active |
| `cmti.risk.v1.CuspState` | `state_id` | 1 | Active |
| `cmti.risk.v1.EvidenceBundle` | `canonical_blake3` | 3 | Active |
| `cmti.risk.v1.EvidenceBundle` | `evidence_bundle_id` | 1 | Active |
| `cmti.risk.v1.EvidenceBundle` | `evidence_item_ids` | 2 | Active |
| `cmti.risk.v1.ForecastServiceSubscribeForecastsRequest` | `assets` | 1 | Active |
| `cmti.risk.v1.ForecastServiceSubscribeForecastsRequest` | `event_kinds` | 2 | Active |
| `cmti.risk.v1.ForecastServiceSubscribeForecastsRequest` | `resume_token` | 3 | Active |
| `cmti.risk.v1.ForecastServiceSubscribeForecastsResponse` | `forecast` | 2 | Active |
| `cmti.risk.v1.ForecastServiceSubscribeForecastsResponse` | `stream` | 1 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `asset` | 2 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `curve` | 5 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `event_kind` | 3 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `evidence_bundle_id` | 7 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `forecast_id` | 1 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `model_package_id` | 8 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `quality` | 6 | Active |
| `cmti.risk.v1.ForecastSnapshot` | `status` | 4 | Active |
| `cmti.risk.v1.GetEvidenceRequest` | `evidence_bundle_id` | 1 | Active |
| `cmti.risk.v1.GetEvidenceResponse` | `evidence` | 1 | Active |
| `cmti.risk.v1.GetForecastRequest` | `forecast_id` | 1 | Active |
| `cmti.risk.v1.GetForecastResponse` | `forecast` | 1 | Active |
| `cmti.risk.v1.GetScenarioRequest` | `forecast_id` | 1 | Active |
| `cmti.risk.v1.GetScenarioResponse` | `scenario` | 1 | Active |
| `cmti.risk.v1.ListRulesResponse` | `rules` | 1 | Active |
| `cmti.risk.v1.ProbabilityPoint` | `baseline_ppm` | 5 | Active |
| `cmti.risk.v1.ProbabilityPoint` | `cumulative_incidence_ppm` | 2 | Active |
| `cmti.risk.v1.ProbabilityPoint` | `horizon_seconds` | 1 | Active |
| `cmti.risk.v1.ProbabilityPoint` | `lower_bound_ppm` | 3 | Active |
| `cmti.risk.v1.ProbabilityPoint` | `upper_bound_ppm` | 4 | Active |
| `cmti.risk.v1.RiskServiceGetCuspStateRequest` | `asset` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetCuspStateResponse` | `cusp_state` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetEvidenceBundleRequest` | `evidence_bundle_id` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetEvidenceBundleResponse` | `evidence` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetForecastSnapshotRequest` | `forecast_id` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetForecastSnapshotResponse` | `forecast` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetScenarioDistributionRequest` | `forecast_id` | 1 | Active |
| `cmti.risk.v1.RiskServiceGetScenarioDistributionResponse` | `scenario` | 1 | Active |
| `cmti.risk.v1.RiskServiceSubscribeForecastsRequest` | `assets` | 1 | Active |
| `cmti.risk.v1.RiskServiceSubscribeForecastsRequest` | `event_kinds` | 2 | Active |
| `cmti.risk.v1.RiskServiceSubscribeForecastsRequest` | `resume_token` | 3 | Active |
| `cmti.risk.v1.RiskServiceSubscribeForecastsResponse` | `forecast` | 2 | Active |
| `cmti.risk.v1.RiskServiceSubscribeForecastsResponse` | `stream` | 1 | Active |
| `cmti.risk.v1.ScenarioDistribution` | `quantiles_ppm` | 2 | Active |
| `cmti.risk.v1.ScenarioDistribution` | `scenario_id` | 1 | Active |
| `cmti.risk.v1.ScenarioDistribution` | `values` | 3 | Active |
| `cmti.risk.v1.SubscribeAlertEventsRequest` | `resume_token` | 2 | Active |
| `cmti.risk.v1.SubscribeAlertEventsRequest` | `rule_ids` | 1 | Active |
| `cmti.risk.v1.SubscribeAlertEventsResponse` | `alert_event` | 2 | Active |
| `cmti.risk.v1.SubscribeAlertEventsResponse` | `stream` | 1 | Active |
| `cmti.risk.v1.UpsertRuleRequest` | `expected_revision` | 2 | Active |
| `cmti.risk.v1.UpsertRuleRequest` | `rule` | 1 | Active |
| `cmti.risk.v1.UpsertRuleResponse` | `rule` | 1 | Active |
| `cmti.settings.v1.CreateExportRequest` | `destination_bookmark_id` | 4 | Active |
| `cmti.settings.v1.CreateExportRequest` | `end_exclusive` | 3 | Active |
| `cmti.settings.v1.CreateExportRequest` | `export_kind` | 1 | Active |
| `cmti.settings.v1.CreateExportRequest` | `start_inclusive` | 2 | Active |
| `cmti.settings.v1.CreateExportResponse` | `export_id` | 1 | Active |
| `cmti.settings.v1.CreateExportResponse` | `exported_records` | 3 | Active |
| `cmti.settings.v1.CreateExportResponse` | `manifest_blake3` | 2 | Active |
| `cmti.settings.v1.GetSettingsResponse` | `settings` | 1 | Active |
| `cmti.settings.v1.SettingsSnapshot` | `configuration_blake3` | 3 | Active |
| `cmti.settings.v1.SettingsSnapshot` | `restart_required` | 4 | Active |
| `cmti.settings.v1.SettingsSnapshot` | `revision` | 2 | Active |
| `cmti.settings.v1.SettingsSnapshot` | `schema_version` | 1 | Active |
| `cmti.settings.v1.UpdateSettingsRequest` | `expected_revision` | 1 | Active |
| `cmti.settings.v1.UpdateSettingsRequest` | `settings` | 2 | Active |
| `cmti.settings.v1.UpdateSettingsResponse` | `settings` | 1 | Active |

## Enum values

| Fully qualified enum | Value | Number | Status |
|---|---|---:|---|
| `cmti.common.v1.AssetNamespace` | `ASSET_NAMESPACE_EVM` | 2 | Active |
| `cmti.common.v1.AssetNamespace` | `ASSET_NAMESPACE_FIAT` | 4 | Active |
| `cmti.common.v1.AssetNamespace` | `ASSET_NAMESPACE_NATIVE` | 1 | Active |
| `cmti.common.v1.AssetNamespace` | `ASSET_NAMESPACE_SOLANA` | 3 | Active |
| `cmti.common.v1.AssetNamespace` | `ASSET_NAMESPACE_SYNTHETIC` | 5 | Active |
| `cmti.common.v1.AssetNamespace` | `ASSET_NAMESPACE_UNSPECIFIED` | 0 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_AVAILABLE` | 1 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_DEGRADED` | 2 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_EXPERIMENTAL` | 3 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_INSUFFICIENT_DATA` | 5 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_MODEL_INCOMPATIBLE` | 7 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_OUT_OF_DISTRIBUTION` | 4 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_SOURCE_UNHEALTHY` | 6 | Active |
| `cmti.common.v1.AvailabilityState` | `AVAILABILITY_STATE_UNSPECIFIED` | 0 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_CAPACITY_EXCEEDED` | 11 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_INCOMPATIBLE_VERSION` | 5 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_INSUFFICIENT_DATA` | 9 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_INTERNAL_INVARIANT_VIOLATION` | 14 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_INVALID_ARGUMENT` | 1 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_MODEL_INCOMPATIBLE` | 8 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_MODEL_UNAVAILABLE` | 7 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_NOT_FOUND` | 4 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_OUT_OF_DISTRIBUTION` | 10 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_PERMISSION_DENIED` | 3 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_REPLAY_NOT_AVAILABLE` | 13 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_SOURCE_UNHEALTHY` | 6 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_STORAGE_PRESSURE` | 12 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_UNAUTHENTICATED` | 2 | Active |
| `cmti.common.v1.ErrorCode` | `ERROR_CODE_UNSPECIFIED` | 0 | Active |
| `cmti.common.v1.StreamDeliveryPolicy` | `STREAM_DELIVERY_POLICY_COALESCE_SUPERSEDED` | 1 | Active |
| `cmti.common.v1.StreamDeliveryPolicy` | `STREAM_DELIVERY_POLICY_LOSSLESS_BOUNDED` | 2 | Active |
| `cmti.common.v1.StreamDeliveryPolicy` | `STREAM_DELIVERY_POLICY_UNSPECIFIED` | 0 | Active |
| `cmti.common.v1.StreamFrameKind` | `STREAM_FRAME_KIND_DELTA` | 2 | Active |
| `cmti.common.v1.StreamFrameKind` | `STREAM_FRAME_KIND_SNAPSHOT` | 1 | Active |
| `cmti.common.v1.StreamFrameKind` | `STREAM_FRAME_KIND_SNAPSHOT_REFERENCE` | 3 | Active |
| `cmti.common.v1.StreamFrameKind` | `STREAM_FRAME_KIND_TERMINAL` | 4 | Active |
| `cmti.common.v1.StreamFrameKind` | `STREAM_FRAME_KIND_UNSPECIFIED` | 0 | Active |
| `cmti.common.v1.StreamTerminalStatus` | `STREAM_TERMINAL_STATUS_CANCELLED` | 3 | Active |
| `cmti.common.v1.StreamTerminalStatus` | `STREAM_TERMINAL_STATUS_COMPLETED` | 2 | Active |
| `cmti.common.v1.StreamTerminalStatus` | `STREAM_TERMINAL_STATUS_FAILED` | 4 | Active |
| `cmti.common.v1.StreamTerminalStatus` | `STREAM_TERMINAL_STATUS_OPEN` | 1 | Active |
| `cmti.common.v1.StreamTerminalStatus` | `STREAM_TERMINAL_STATUS_UNSPECIFIED` | 0 | Active |
| `cmti.health.v1.ServingStatus` | `SERVING_STATUS_NOT_SERVING` | 2 | Active |
| `cmti.health.v1.ServingStatus` | `SERVING_STATUS_SERVING` | 1 | Active |
| `cmti.health.v1.ServingStatus` | `SERVING_STATUS_UNSPECIFIED` | 0 | Active |
| `cmti.market.v1.SnapshotHealth` | `SNAPSHOT_HEALTH_DEGRADED` | 2 | Active |
| `cmti.market.v1.SnapshotHealth` | `SNAPSHOT_HEALTH_HEALTHY` | 1 | Active |
| `cmti.market.v1.SnapshotHealth` | `SNAPSHOT_HEALTH_STALE` | 3 | Active |
| `cmti.market.v1.SnapshotHealth` | `SNAPSHOT_HEALTH_UNAVAILABLE` | 4 | Active |
| `cmti.market.v1.SnapshotHealth` | `SNAPSHOT_HEALTH_UNSPECIFIED` | 0 | Active |
| `cmti.replay.v1.ReplayControlAction` | `REPLAY_CONTROL_ACTION_PAUSE` | 1 | Active |
| `cmti.replay.v1.ReplayControlAction` | `REPLAY_CONTROL_ACTION_RESUME` | 2 | Active |
| `cmti.replay.v1.ReplayControlAction` | `REPLAY_CONTROL_ACTION_SEEK` | 3 | Active |
| `cmti.replay.v1.ReplayControlAction` | `REPLAY_CONTROL_ACTION_STOP` | 4 | Active |
| `cmti.replay.v1.ReplayControlAction` | `REPLAY_CONTROL_ACTION_UNSPECIFIED` | 0 | Active |
| `cmti.risk.v1.ForecastStatus` | `FORECAST_STATUS_ABSTAINED` | 2 | Active |
| `cmti.risk.v1.ForecastStatus` | `FORECAST_STATUS_ACTIVE` | 1 | Active |
| `cmti.risk.v1.ForecastStatus` | `FORECAST_STATUS_EXPERIMENTAL` | 3 | Active |
| `cmti.risk.v1.ForecastStatus` | `FORECAST_STATUS_SUPPRESSED` | 4 | Active |
| `cmti.risk.v1.ForecastStatus` | `FORECAST_STATUS_UNSPECIFIED` | 0 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_BASIS_DISLOCATION` | 6 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_CONTAGION` | 8 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_DOWNSIDE` | 1 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_LIQUIDATION_CASCADE` | 5 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_LIQUIDITY_VACUUM` | 4 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_STABLECOIN_DISLOCATION` | 7 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_UNSPECIFIED` | 0 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_UPSIDE_SQUEEZE` | 2 | Active |
| `cmti.risk.v1.TransitionEventKind` | `TRANSITION_EVENT_KIND_VOLATILITY_EXPLOSION` | 3 | Active |

## Reserved message field numbers

| Fully qualified message | Number |
|---|---:|

## Reserved message field names

| Fully qualified message | Name |
|---|---|

## Reserved enum numbers

| Fully qualified enum | Number |
|---|---:|

## Reserved enum names

| Fully qualified enum | Name |
|---|---|
