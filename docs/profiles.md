# Profiles

A profile (`profiles/<id>.json`, or `POST /v1/profile {"profile": {...}}`) describes the printer:
identity, speed, formats, ticket values, bins, consumables, start-up conditions. Schema 1 (the old
flat file, no `schema_version`) still loads and is upgraded; new files use `"schema_version": 2`.
Unknown keys are refused.

## The smallest valid profile

```json
{"schema_version": 2, "id": "smallest", "label": "Smallest"}
```

Only `schema_version`, `id` (file stem; letters, digits, `-`, `_`, `.`) and `label` are required.
Everything else takes a default that is consistent with the rest of the document:

| Field | Default |
|-------|---------|
| `description.color_modes` | `["monochrome", "color"]` |
| `description.pages_per_minute` | 20 |
| `description.pages_per_minute_color` | **follows the colour modes**: 12 when `color` is among the modes, none for a monochrome-only printer. An explicit value on a monochrome-only printer is an error |
| `description.event_rate`, `multiple_document_jobs` | 30 s, `true` |
| `print.sides` | `["OneSided"]` (two-sided values switch the finishings section on) |
| `default_ticket.*` | the usual value if it is in the matching list (Letter, stationery, Portrait, Normal, OneSided, 600 dpi), else the list's first entry |
| `inputs` / `outputs` | one `Tray1` (Letter) / one `Bin1`; `[]` publishes none |
| `versions` | 1.0 only (`v11`, `v12`, `v20` all `false`) |

A monochrome-only printer needs one more key:

```json
{"schema_version": 2, "id": "mono", "label": "Mono",
 "description": {"color_modes": ["monochrome"], "pages_per_minute": 30}}
```

The unit tests load the smallest document for every combination of colour modes, sides and
version flags and check that each published section validates against the WS-Print schema.
