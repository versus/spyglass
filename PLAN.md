# spyglass — безопасная замена Agent-Reach

> Основа: аудит `Panniantong/Agent-Reach` @ `94f06c1` (v1.5.0, MIT). Код изучен статически.
> Ревизия 4: один CLI на **Rust**, без Docker, без Python-рантайма; видимый «браузер агента»;
> только EU/US-инструменты; транскрипция — опциональная и редкая.

---

## 1. Выводы аудита (кратко)

Agent-Reach — установщик + `doctor` + `SKILL.md`, который учит агента вызывать ~10
сторонних CLI. Закладок не найдено; проблемы архитектурные:

| # | Проблема | Как решаем |
|---|---|---|
| C1 | Установка/обновление через remote-markdown из `main` (агент выполняет `install.md`/`update.md`) | skill вшит в бинарь, обновление только через подписанный релиз |
| C2 | Пакеты без пинов (`main.zip`, `pipx install twitter-cli`, `npm -g …`, docker `latest`) | `Cargo.lock` + `cargo-vet`; внешние бинари по версии + sha256 |
| C3 | Модель сама пишет shell/`python -c` из недоверенных строк | агент вызывает только `spyglass <platform> <verb> <arg>`; наш CLI зовёт upstream через argv |
| C4 | OpenCLI управляет основным залогиненным Chrome | отдельный видимый «браузер агента» с фиксированными read-only сценариями (§4) |
| C5 | Контент отдаётся агенту без маркировки | конверт `untrusted` + санитизация + лимит размера |
| H1 | Все URL уходят в `r.jina.ai` | локальный fetch + извлечение текста |
| H3/H4 | Cookies в plaintext + авто-извлечение из браузера | keyring для ключей; логины живут только в профиле браузера агента |
| H6 | `check-update`/`watch` кладут release notes с GitHub в контекст агента | не переносим |
| — | skill учит `gh issue/pr/repo/release create` при заявленном «read-only» | write-verbs отсутствуют в парсере |

## 2. Ключевые решения

| Решение | Почему |
|---|---|
| **CLI, не набор MCP-инструментов** | 20+ схем съедают контекст малых моделей; CLI с `--help` по требованию ≈ 0 токенов |
| **Rust** | один статический бинарь без рантайма; memory safety; строгие типы на границах ввода; удобная подпись/дистрибуция |
| **Без Docker** | не у всех есть; всё работает на голой системе |
| **Без Python** | `gh`/`feedparser`/`trafilatura` заменяем Rust-крейтами; `yt-dlp` — его официальный standalone-бинарь |
| **Свой CDP-клиент через pipe вместо Playwright** | мы не кликаем, а перехватываем JSON — нужен малый срез CDP; pipe = нет TCP-порта; нет Node.js |
| **Видимый браузер агента** | прозрачность, человек решает логин/капчу/2FA, меньше детекта |

## 3. Архитектура

```
Agent ──reads──▶ SKILL.md (≤ ~800 токенов) ──▶ spyglass <platform> --help (по требованию)
  │
  └─ shell ─▶ spyglass <platform> <verb> [args] [--max-chars N] [--json]
                 │
                 ├─ clap-парсер: только объявленные verbs (write-verbs не существуют)
                 ├─ validate: URL / handle / id / query → newtype’ы
                 ├─ policy: платформа включена? лимиты, темп
                 ├─ один из бэкендов:
                 │    ├─ net    — reqwest+rustls, SSRF-safe resolver   (web, rss, github, search, ytsearch?)
                 │    ├─ tool   — внешний бинарь, argv, `--`, sandbox   (yt-dlp, whisper.cpp)
                 │    └─ browser — демон + Chrome через CDP-pipe         (x, reddit, instagram, web --render)
                 └─ output: нормализация → санитизация → обрезка →
                            <untrusted source="x" url="…"> … </untrusted>
```

### Cargo workspace

```
crates/
  spyglass-cli/        # clap, команды, форматирование вывода
  spyglass-core/       # newtype-валидаторы, policy, output/санитизация, audit (JSONL), ошибки
  spyglass-net/        # reqwest (rustls), кастомный dns::Resolve (фильтр не-global IP + pinning),
                   # redirect-policy с ревалидацией, лимиты размера/времени, декомпрессия с лимитом
  spyglass-secrets/    # keyring (Secret Service / Keychain / Credential Manager)
  spyglass-sandbox/    # запуск внешних бинарей: Linux — landlock + seccomp + rlimits;
                   # macOS — sandbox-exec профиль; env allowlist; таймаут; лимит вывода
  spyglass-browser/    # CDP-over-pipe клиент (--remote-debugging-pipe, fd 3/4), демон, очередь задач,
                   # Fetch-перехват (allowlist), Network.getResponseBody, pause/resume, хэндофф
  spyglass-platforms/  # web, rss, github, search, youtube, x, reddit, instagram, transcribe
  spyglass-skill/      # SKILL.md и references/*.md, вшитые через include_str!
```

Основные крейты (каждый проходит `cargo-vet`): `clap`, `tokio`, `reqwest`+`rustls`,
`serde`/`serde_json`, `url`, `hickory-resolver`, `feed-rs`, `dom_smoothie` (порт Mozilla
Readability) + `htmd` (HTML→Markdown), `keyring`, `landlock`, `tracing`, `unicode-normalization`.
Во всех наших крейтах — `#![forbid(unsafe_code)]` (кроме изолированного места для pipe-fd в `spyglass-browser`).

## 4. Браузер агента (видимый, отдельный профиль)

```
spyglass browser start         # демон + видимое окно Chrome (отдельный user-data-dir)
spyglass browser login x       # вкладка логина; пользователь входит сам (2FA/капча)
spyglass x search "query"      # CLI → unix-socket (0600) → демон → вкладка «🤖 spyglass: x search»
                           #   → перехват JSON сайта → нормализация → untrusted-конверт
spyglass browser pause|resume  # перехват управления
spyglass browser stop
```

- **Chrome — системный** (Chrome/Chromium из пакетного менеджера пользователя); пиним минимальную версию, не скачиваем сами.
- **CDP через `--remote-debugging-pipe`**: никакого TCP-порта. CLI ↔ демон — unix-socket 0600 (Windows — named pipe с ACL).
- **Нужный срез CDP**: `Target.createTarget/attachToTarget`, `Page.navigate`, `Fetch.enable` (allowlist доменов, блок медиа),
  `Network.responseReceived/getResponseBody`, `Runtime.evaluate` (только наши встроенные скрипты, напр. Readability.js), `Browser.close`.
- **Два контекста**: `accounts` (постоянный, с логинами, только для сценариев x/reddit/instagram) и
  `scratch` (`Target.createBrowserContext`, эфемерный, без cookies — для произвольных URL и `web read --render`).
- **Только чтение**: агент передаёт запрос/URL/handle; не JS, не клики; сценарии не нажимают кнопки действий.
- **Вмешательство**: `pause` — терминал/хоткей/активность пользователя во вкладке; `resume` — только терминал, не страница.
- **Хэндофф**: стена логина / капча / checkpoint → вкладка на передний план, уведомление, ожидание N минут,
  агенту статус `waiting_for_user`.
- **Режимы**: `headed` (десктоп, по умолчанию) · `headless` (сервер, без хэндоффа).
- **Гигиена профиля**: без Google-синка, расширений, менеджера паролей и автозаполнения.
- **Темп**: очередь на домен, паузы, дневные лимиты на платформу.

| Угроза | Защита |
|---|---|
| Prompt-injection → «напиши пост/DM» | write-сценариев нет; агент не управляет кликами |
| Вредная страница из `web read` атакует сессии | открывается в `scratch` без cookies |
| Страница подделывает «resume» | resume только вне страницы |
| Локальный процесс перехватывает браузер | нет CDP-порта; сокет 0600 |
| Бан аккаунта | рекомендуем служебные аккаунты; лимиты темпа; предупреждение о ToS при первом `login` |

## 5. Платформы

| Платформа | Бэкенд | Внешняя зависимость | Авторизация | Verbs |
|---|---|---|---|---|
| **Web** | net + Readability/htmd | — | — | `read <url>` |
| ↳ JS-страницы | browser `scratch` + Readability.js | Chrome | — | `read --render <url>` |
| **Поиск** | browser `scratch` → DuckDuckGo HTML (по умолчанию; DDG отсекает не-браузерные TLS-клиенты) | Chrome | — | `search <query>` |
| ↳ платно (opt-in) | net → Brave Search API ($5/1000, кредит $5/мес) или Exa | — | API-ключ | `search --provider brave\|exa` |
| **RSS/Atom** | net + `feed-rs` | — | — | `rss <url>` |
| **GitHub** | net → REST API напрямую (без `gh`) | — | опц. токен (keyring) | `repo`, `file`, `search-repos`, `search-code`, `issues`, `issue`, `prs`, `pr`, `releases`, `runs` |
| **YouTube** | tool → `yt-dlp` standalone-бинарь | yt-dlp (Unlicense), sha256-пин | — | `video`, `transcript`, `search`, `comments` |
| **X** | browser `accounts` | Chrome | вход пользователя | `search`, `post`, `user`, `timeline` |
| **Reddit** | browser (без логина, при необходимости — с логином) | Chrome | не нужна | `search`, `sub`, `post`, `sub-info` |
| **Instagram** | browser `accounts` | Chrome | вход пользователя | `profile`, `posts`, `post` |
| **Транскрипция** (опц.) | tool → `whisper-cli` (whisper.cpp) | whisper.cpp (MIT) + модель | — | `transcribe <url\|file>` |
| ↳ облако (opt-in) | net → Groq / OpenAI Whisper API | — | API-ключ | `transcribe --cloud` |

Убрано: Bilibili, XiaoHongShu, Xueqiu, Xiaoyuzhou, V2EX, Boss直聘, Facebook, LinkedIn; все их CLI;
Jina Reader; Docker-зависимости (SearXNG только как внешний URL, если у пользователя уже есть).

### Транскрипция — политика нагрузки
- Выключена по умолчанию; модель скачивается только по явному `spyglass transcribe --setup` (sha256-пин).
- Запускается **только если у видео нет субтитров** (большинству YouTube-видео она не нужна).
- `nice`/`ionice`, ограничение потоков (по умолчанию половина ядер), лимит длительности аудио.
- Модель по умолчанию — `base`/`small`; `large-v3-turbo` — только при наличии GPU (Metal/CUDA/Vulkan).
- Декодирование: аудио берём у yt-dlp сразу в m4a и декодируем Rust-крейтом `symphonia` (без ffmpeg); ffmpeg — запасной путь.

## 6. Контекст для малых моделей
- `SKILL.md` ≤ ~800 токенов: таблица «намерение → команда» + 5 правил.
- Детали — `spyglass <platform> --help` или `references/<platform>.md` (≤ ~300 токенов), только по требованию.
- Вывод по умолчанию компактный: Markdown, `--max-chars 8000`, списки ≤ 10; `--json` по запросу.
- Без «MUST USE» в описании skill.
- Опционально: **один** MCP-инструмент `spyglass(command: str)` для агентов без shell.

## 7. Безопасность — непреложные правила
1. Нет remote-инструкций, авто-обновлений, cron.
2. Нет shell: только argv, `--` перед пользовательскими данными, путь к бинарю фиксирован, sha256 сверяется при запуске.
3. Write-verbs не существуют в CLI; браузерные сценарии не нажимают кнопки действий.
4. Ключи API — только keyring; в env конкретного вызова; не в argv/логах. Логины — только в профиле браузера агента.
5. Внешние бинари — в sandbox (landlock/seccomp или sandbox-exec), временный HOME, урезанное env, таймаут, лимит вывода.
6. SSRF: резолв → отказ на не-global IP → соединение на проверенный IP → ревалидация каждого redirect.
7. Вывод = недоверенные данные: санитизация (ANSI, bidi, zero-width, управляющие), обрезка, конверт.
8. `doctor` без побочных эффектов.
9. Аудит-лог JSONL: время, платформа, verb, хост, размер, статус.

## 8. Supply chain, сборка, CI
- `Cargo.lock`, сборка только с `--locked`; `cargo-deny` (advisories, лицензии, разрешённые источники, бан дубликатов);
  `cargo-audit`; `cargo-vet` (ревью каждой зависимости); минимум зависимостей.
- Внешние бинари (`yt-dlp`, `whisper-cli`, модели) — манифест `tools.toml`: версия, URL релиза, sha256. `spyglass tools install/verify`.
- Actions запинены по SHA, `permissions: read-all`, без `pull_request_target`.
- Релизы: `cargo-dist`, воспроизводимая сборка, подпись (Sigstore/minisign), SBOM (`cargo-cyclonedx`).
- Дистрибуция: GitHub Releases, `cargo install --locked`, Homebrew tap, AUR.

## 9. Обязательные тесты
- SSRF: `0x7f.1`, `[::ffff:127.0.0.1]`, `169.254.169.254`, DNS-rebinding, redirect на приватный IP, не-http схемы.
- Инъекции аргументов: `"`, `'`, `$()`, `` ` ``, `;`, `\n`, NUL, `--exec=…` → ровно один argv-элемент после `--`.
- Write-verbs: `spyglass x post …` — ошибка парсера.
- Браузер: произвольный URL никогда не открывается в `accounts`; запрос на неразрешённый домен блокируется; resume со страницы игнорируется.
- Санитизация: bidi-override, zero-width, ANSI, гигантский вывод, gzip-бомба.
- Секреты: маркер-строка не появляется в stdout/stderr/audit/ошибках.
- Fuzz (`cargo-fuzz`) для валидаторов URL/handle и парсеров CDP-сообщений.
- Контрактные тесты платформ на записанных фикстурах; ноль сети в CI.

## 10. Дорожная карта

| Фаза | Содержание | Готово, когда |
|---|---|---|
| 0. Каркас | workspace, clap-скелет, spyglass-core, spyglass-net (SSRF), spyglass-secrets, spyglass-sandbox, audit, CI (deny/audit/vet) | тесты §9 для net/validate/output зелёные |
| 1. Без аккаунтов | web, search, rss, github, youtube (yt-dlp) + SKILL.md | малая модель решает исследовательскую задачу только через `spyglass` |
| 1.5. Прототип браузера | CDP-pipe клиент, демон, видимое окно, Reddit без логина, X с логином, хэндофф | оба сценария проходят на реальном IP пользователя |
| 2. Браузерные платформы | x, reddit, instagram; pause/resume; `scratch` для `web read --render` | write-сценарии недостижимы; произвольный URL не попадает в `accounts` |
| 3. Опционально | транскрипция (whisper.cpp / облако), MCP-обёртка `spyglass(command)`, headless-режим для серверов | — |

## 11. Лицензия
MIT разрешает переписывание; код Agent-Reach не переносим. Внешние инструменты (yt-dlp — Unlicense,
whisper.cpp — MIT) вызываются отдельными процессами. Встроенный Readability.js — Apache-2.0 → указать в `THIRD_PARTY_NOTICES`.
