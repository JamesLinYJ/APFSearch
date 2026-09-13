# Localization

Application strings use stable semantic identifiers, for example:

```swift
L("search.all_files")
L("action.cancel")
LT("export.completed_count", .integer(count))
```

Identifiers describe a string's purpose. Changing Chinese or English wording
does not change its identifier. Add translations to
`Resources/Localizable.xcstrings`, `Features.xcstrings`, and `InfoPlist.xcstrings`.
The supported languages are `en`, `zh-Hans`, `zh-Hant`, `ja`, `ko`, `ru`, `es`,
and `pt`. The catalog's
source language and the app's development region are `en`. Unsupported app languages
fallback to English through Foundation; identifiers are independent of wording.
Keep the system-defined keys in `InfoPlist.xcstrings` unchanged.

Bundle language declarations are derived from `InfoPlist.xcstrings`, and the
catalog checks require complete, matching language coverage across all tables.
Spanish and Portuguese use the base `es` and `pt` localizations; Foundation also
selects these for regional preferences such as `es-MX` and `pt-BR`. Portuguese
wording currently follows European Portuguese conventions. A separate Brazilian
Portuguese translation can be added as `pt-BR` when needed.

Keep query operators, command names, paths, format specifiers, and numeric limits
intact. Use positional placeholders when a language needs a different word order.
Count displays currently use labels (such as “Records exported: %@”) rather than
sentences whose grammar depends on a count. If adding count-dependent prose, use
native String Catalog plural variations and pass a numeric argument; do not build
language-specific suffixes or plural rules in Swift.

`NSLocalizedString` and `Bundle.main` select the application's localization.
Numbers and dates use the user's regional preferences independently. The app,
CLI, and service use the same compiled catalog. XPC messages carry stable keys
and typed arguments so the receiving process can render its own language;
file paths, user names, and technical error text are not reverse-translated.

`Resources/LocalizationKeyMigration.json` records the completed source migration.
It is not copied into the app and is never read at runtime. The migration script
uses this fixed mapping; it does not create keys again from edited translations.

Run the String Catalog and service-protocol checks with:

```sh
python3 tests/check_localization.py --report validation/localization.json
python3 tests/check_localization_protocol.py
```

`tests/run.sh` and `tests/run_search_window_tests.py` put their executables in temporary
`.app` bundles with compiled catalogs. Running their unbundled intermediates
does not validate localized UI. Test language overrides are process arguments;
tests never change the user's system language or patch Foundation.
