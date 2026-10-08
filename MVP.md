# Критерии рабочего MVP

MVP считается готовым, когда **каждый** пункт ниже проверен на этой машине
(реальная сеть, реальный Chrome) и автотестами там, где это возможно без сети.

## A. Сборка и качество
- [x] A1. `cargo build --release --locked` даёт один бинарь `spyglass`.
- [x] A2. `cargo test --locked` зелёный, тесты без сети.
- [x] A3. `cargo clippy --all-targets -- -D warnings` чистый; `#![forbid(unsafe_code)]` во всех крейтах, кроме изолированного модуля pipe-fd.
- [x] A4. Есть `deny.toml` и CI-workflow (fmt, clippy, test, cargo-deny), actions запинены по SHA.

## B. Каналы без аккаунтов
- [x] B1. `spyglass web read <url>` — чистый Markdown статьи, заголовок, без меню/скриптов.
- [x] B2. `spyglass rss <url>` — последние записи ленты (заголовок, дата, ссылка, кратко).
- [x] B3. `spyglass github repo|readme|file|search-repos|issues|issue|prs|pr|releases` по публичному API без токена; с токеном из keyring/env — тоже.
- [x] B4. `spyglass youtube video|transcript|search` через системный `yt-dlp` (субтитры без скачивания медиа).
- [x] B5. `spyglass search <query>` — DuckDuckGo через браузер агента без ключа (капча → хэндофф); Brave/Exa по ключу опционально.

## C. Браузер агента
- [x] C1. `spyglass browser start|status|stop` — демон + видимое окно Chrome с отдельным профилем; CLI↔демон через unix-socket 0600; Chrome управляется через `--remote-debugging-pipe` (TCP-порта нет — проверяется).
- [x] C2. `spyglass browser login <x|reddit>` открывает страницу входа во вкладке; пользователь входит сам.
- [x] C3. `spyglass reddit search|sub|post` работает через браузер без логина.
- [-] C4. `spyglass x search|post|user` работает через браузер после логина пользователя.
- [x] C5. Стена логина/капча → статус `waiting_for_user`, вкладка выходит вперёд, ожидание с таймаутом (хэндофф).
- [x] C6. Сетевой allowlist на задачу: запросы к чужим доменам блокируются; медиа блокируются.
- [x] C7. `spyglass web read --render <url>` идёт в эфемерный контекст без cookies.

## D. Безопасность (автотесты)
- [x] D1. SSRF: `localhost`, `127.0.0.1`, `0x7f.1`, `[::1]`, `[::ffff:127.0.0.1]`, `169.254.169.254`, `10/8`, `192.168/16`, не-http схемы, userinfo в URL — отказ; DNS-ответ с приватным IP — отказ; redirect на приватный адрес — отказ.
- [x] D2. Аргументы с `"`, `'`, `$()`, `` ` ``, `;`, `\n`, NUL, `--exec=…` доходят до yt-dlp ровно одним argv-элементом после `--` или отвергаются валидатором.
- [x] D3. Write-команд нет: `spyglass x post-tweet`, `spyglass github pr-create` и т.п. — ошибка парсера.
- [x] D4. Санитизация вывода: ANSI-escape, bidi-override, zero-width, управляющие символы удаляются; вывод обрезается по `--max-chars`; всё обёрнуто в `<untrusted …>`.
- [x] D5. Секреты не попадают в stdout/stderr/audit (тест с маркер-строкой); ответы ограничены по размеру.

## E. Агентный опыт
- [x] E1. `spyglass skill` печатает встроенный `SKILL.md` (≤ ~800 токенов); `spyglass skill install --claude` кладёт его в `~/.claude/skills/spyglass/` только по явной команде.
- [x] E2. `spyglass doctor` — без побочных эффектов показывает, что готово (yt-dlp, Chrome, ключи, демон).
- [x] E3. `spyglass secrets set|rm|list` — ключи в системном keyring, ввод скрыт / из stdin.
- [x] E4. Аудит-лог JSONL в `~/.local/state/spyglass/audit.jsonl` без секретов и тел ответов.
- [x] E5. Сквозная проверка: агент (Claude Code) с одним только skill выполняет задачу «найди и кратко опиши обсуждение X на Reddit + README репо Y + субтитры видео Z».

## Вне MVP (следующие итерации)
Instagram, транскрипция (whisper.cpp), landlock/seccomp-песочница для yt-dlp (в MVP — урезанное env,
таймаут, лимит вывода), pause/resume, headless-режим для серверов, MCP-обёртка, подписанные релизы.

## Статус (2026-10-08)
- [x] — проверено на этой машине и/или автотестами (75 тестов + 1 с реальным Chrome).
- [x] B5 — по умолчанию DuckDuckGo через браузер агента (без ключа), с хэндоффом капчи; Brave/Exa — опционально, проверены только на фикстурах.
- [-] C4 — отложено пользователем (не приоритет, 2026-10-08): код, парсер и хэндофф логина готовы, на живых данных X не проверено.
- A4: `cargo deny` локально не запускался (нет в системе) — выполняется в CI.
- A3: в крейте нет ни одного `unsafe` — стоит `#![forbid(unsafe_code)]`; pipe-fd передаёт `command-fds`.
