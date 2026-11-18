# Oura Cloud API v2 coverage from ring data

For every field the Oura Cloud API v2 returns (spec 1.43, the fields observed in a
full account backup made with `tools/oura_cloud_backup.py`), where the same
information comes from when the ring is read directly with this project. The aim is
that nothing Oura's cloud shows is silently lost: a field is either in the raw
events, derivable from them, produced by a ring feature that must be switched on,
or genuinely not ring data.

Status legend:

- **raw**: carried directly by history events we already store.
- **derive**: computable from stored events (an algorithm to port or write; no new
  data needed). Some steps need Oura's proprietary models (not in this repo) or a
  replacement of our own.
- **feature**: the ring can measure it, but only when a ring feature is enabled.
  A factory-reset ring paired with this client starts with these **off** (see
  "Capture gaps" below).
- **app/user**: not a sensor measurement: user-entered data, app state, or account
  metadata. Needs our own input (for example an annotations table) if wanted.
- **cloud-only**: computed only by Oura's servers.

## Per endpoint

| Endpoint | Field(s) | Ring source | Status |
| --- | --- | --- | --- |
| `heartrate` | `timestamp`, `bpm`, `source=awake/rest/sleep` | `green_ibi_quality` 0x80 (day), `ibi_and_amplitude` 0x60 (night), `hrv_event` 0x5d (5-min HR) | raw |
| `heartrate` | `source=workout` (dense workout HR) | `ehr_trace` 0x73, `ehr_acm_intensity` 0x74 | **feature** `exercise_hr` |
| `heartrate` | `source=live` | live IBI stream (`oura live-hr`) | raw (on demand) |
| `ring_battery_level` | `level` | `debug_data` 0x61 subtype 0x24 (battery %, mV) | raw |
| `ring_battery_level` | `charging`, `in_charger` | `state_change` 0x45 charger transitions | derive |
| `ring_configuration` | `firmware_version`, `hardware_type` | `DeviceInfo`, `ring_start` 0x41 | raw |
| `ring_configuration` | `color`, `design`, `size` | product code (`oura_protocol::device` product-code decode; not yet persisted by `sync`) | raw (wire it into the store) |
| `ring_configuration` | `set_up_at` | account metadata | app/user |
| `personal_info` | `age`, `weight`, `height`, `biological_sex`, `email` | user profile; the ring only keeps an anthropometric copy (`user_information` 0x5c, reset to firmware defaults by a factory reset) | app/user |
| `sleep` | `bedtime_start`, `bedtime_end`, `time_in_bed`, `period`, `type` | `bedtime_period` 0x76 (start/end in ring time, mapped by `ring_time`) | derive |
| `sleep` | `heart_rate`, `hrv` (5-min series), `average_heart_rate`, `lowest_heart_rate`, `average_hrv` | `hrv_event` 0x5d (HR + RMSSD every 5 min), IBI 0x60 | raw / derive |
| `sleep` | `average_breath` | respiratory rate from overnight IBI (respiratory sinus arrhythmia); Oura computes it on the phone (`breathing_rate_calculate_averages`). Ring 5 sends no `sleep_period_information_2` 0x6a | derive |
| `sleep` | `movement_30_sec`, `restless_periods` | `sleep_acm_period` 0x72, `motion_event` 0x47, `motion_period` 0x6b | derive |
| `sleep` | `sleep_phase_5_min`, `sleep_phase_30_sec`, `app_sleep_phase_5_min`, `deep/light/rem_sleep_duration`, `awake_time`, `total_sleep_duration`, `efficiency`, `latency` | needs a hypnogram: Ring 5 emits no `sleep_phase_*` events, so stages come from Oura's SleepNet model (`tools/run_sleep_model.py`, model not included) or a signal-based estimate (open_health's fallback) | derive |
| `sleep` | `readiness`, `readiness_score_delta`, `sleep_score_delta` | scores below | derive |
| `sleep` | `low_battery_alert` | battery | derive |
| `sleep` | `sleep_algorithm_version`, `sleep_analysis_reason`, `ring_id` | Oura processing metadata | app/user |
| `daily_sleep` | `score`, contributors (`deep_sleep`, `efficiency`, `latency`, `rem_sleep`, `restfulness`, `timing`, `total_sleep`) | sleep metrics + calibrated weights (`docs/algorithms/score-weights.md`) | derive |
| `daily_readiness` | `temperature_deviation`, `temperature_trend_deviation` | `sleep_temp_event` 0x75 / `temp_event` 0x46 / `temp_period` 0x69 → `oura_analysis::temperature` nightly value − personal baseline | derive |
| `daily_readiness` | `score`, contributors (`activity_balance`, `body_temperature`, `hrv_balance`, `previous_day_activity`, `previous_night`, `recovery_index`, `resting_heart_rate`, `sleep_balance`, `sleep_regularity`) | baselines over nightly HR/HRV/temperature/sleep/activity (`docs/algorithms/daily-summaries-and-baselines.md`) | derive |
| `daily_activity` | `met` series, `class_5_min`, `high/medium/low/sedentary_met_minutes` and `_time`, `average_met_minutes`, `resting_time`, `non_wear_time`, `inactivity_alerts` | `activity_information` 0x50 (MET bins), `wear_event` 0x53, `state_change` 0x45 | derive |
| `daily_activity` | `steps`, `equivalent_walking_distance` | `real_step_event_feature_1/2` 0x7e/0x7f, paired: 27 quantized gait values per ~30 s (`unpack27` in `tools/run_activity_model.py`). Oura's `steps_motion_decoder_2_0_0` model (not included) turns each pair into three 10 s sub-windows of 11 gait features including cadence (verified on a run: steady ~2.7 Hz). No repo counts steps from these yet: cadence x walking time is the candidate method. Oura's `step_counter` model needs raw accelerometer RData (entitlement-locked). open_health's daily `steps` is a MET heuristic (105 steps/min at MET >= 2.5, 150 at MET >= 7), not from these events | **feature** `real_steps`, then derive (method open) |
| `daily_activity` | `active_calories`, `total_calories` | MET + profile (`oura_analysis::metabolic`) | derive (needs profile) |
| `daily_activity` | `target_calories`, `target_meters`, `meters_to_target`, `score`, contributors | Oura's adaptive goal is not on the ring; needs our own targets | derive (own goals) |
| `daily_spo2` | `spo2_percentage.average` | `spo2_r_pi_event` 0x8b R-ratio → per-hardware quadratic (`docs/spo2-calibration.md`) | derive |
| `daily_spo2` | `breathing_disturbance_index` | Oura's `sleepnet_bdi` model, or our own SpO2-dip index from 0x8b | derive |
| `daily_stress` | `stress_high`, `recovery_high`, `day_summary` | Oura's stress model over daytime HR/HRV/motion, or our own | derive |
| `daily_resilience` | `level`, contributors (`sleep_recovery`, `daytime_recovery`, `stress`) | built on stress + sleep recovery | derive |
| `daily_cardiovascular_age` | `vascular_age`, `pulse_wave_velocity` | `cva_raw_ppg_data` 0x81 raw PPG → CVA model (`docs/cva-cardiovascular-age.md`) | **feature** `cva_ppg` |
| `vO2_max` | `vo2_max` | `oura_analysis::metabolic::vo2max_jackson` (profile + resting HR) | derive (needs profile) |
| `sleep_time` | `optimal_bedtime`, `recommendation`, `status` | sleep history | derive |
| `workout` | `start_datetime`, `end_datetime`, `intensity`, `calories`, `distance` | MET bouts (0x50) + exercise HR + steps | derive (needs `exercise_hr`, `real_steps`) |
| `workout` | `activity` (type), `label`, `source=autodetected` | workout classification (`POST /api/activity-tagging/v2`) | **cloud-only** |
| `workout` | `source=manual` | user-entered in the app | app/user |
| `session` | all | guided/unguided sessions started in the Oura app | app/user |
| `tag`, `enhanced_tag` | all | user-entered tags | app/user (needs an annotations table) |
| `rest_mode_period` | all | user-toggled mode in the app | app/user |

**Truly missing** (not recoverable from ring data at all): automatic workout
activity-type classification (cloud-only), and the app/user rows above. Everything
else is in the events or derivable from them, provided the three features below
are on.

## Capture gaps: features a reset ring starts with off

A factory reset restores the firmware feature modes, and the official app's
post-onboarding enable sequence (`docs/ring-features.md`) never runs for a ring
paired with this client. On a Ring 5 (firmware 2.2.2) after reset and `oura pair`,
`oura feature-status` read:

| feature | after reset | cloud data that depends on it |
| --- | --- | --- |
| `daytime_hr` (0x02) | AUTOMATIC | `heartrate` |
| `resting_hr` (0x08) | AUTOMATIC | `sleep` resting HR |
| `spo2` (0x04) | **OFF** after pairing (enable with `oura features --enable-spo2`) | `daily_spo2` |
| `exercise_hr` (0x03) | **OFF** | `heartrate` `source=workout`, `workout` |
| `real_steps` (0x0b) | **OFF** | `daily_activity.steps`, distance |
| `cva_ppg` (0x0d) | **OFF** | `daily_cardiovascular_age` |
| `experimental` (0x0c) | OFF | none visible (see `docs/ring-features.md`) |

If the account's cloud history contains workout-sourced heart rate, steps or
cardiovascular age, the official app had those features on; turn them back on to
keep capturing the same data:

```sh
oura --key-file key.hex feature-mode real_steps --mode automatic   # first: exercise HR depends on it
oura --key-file key.hex feature-mode exercise_hr --mode automatic
oura --key-file key.hex feature-mode cva_ppg --mode automatic
oura --key-file key.hex feature-status
```

## Ring measurements the Cloud API does not expose

Stored raw and available for modelling, but with no Cloud API counterpart:

- beat-to-beat intervals with PPG amplitude and quality flags, day and night
  (0x80, 0x60), not just 5-minute averages;
- SpO2 **perfusion index** alongside every R-ratio sample (0x8b);
- three skin-temperature streams: daytime samples (0x46), 30-minute periods (0x69)
  and sleep samples (0x75), as absolute °C (the API only gives deviation);
- accelerometer detail: per-event orientation, x/y/z and intensity (0x47), motion
  periods (0x6b) and sleep accelerometer statistics (0x72);
- wear and charger state transitions (0x53, 0x45) and battery voltage (0x61).
