# Localization

Application strings use stable semantic identifiers, for example:

```swift
L("search.all_files")
L("action.cancel")
LT("export.completed_count", .integer(count))
```

Identifiers describe a string's purpose. Changing Chinese or English wording
does not change its identifier. Add translations to
`Resources/Localizable.xcstrings` for `zh-Hans`, `en`, and `zh-Hant`. The catalog's
source language and the app's development region are `en`. Unsupported app languages
fallback to English through Foundation; identifiers are independent of wording.
Keep the system-defined keys in `InfoPlist.xcstrings` unchanged.

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
