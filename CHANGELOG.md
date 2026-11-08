# Changelog

## 0.1.0

Erste Version: Rust-Neuimplementierung von dsh-cli 0.1.0 (Commit `46d5ff8`).

- Alle Befehle des Originals: `run`, `batch`, `research`, `materials`, `status`, `ls`, `wait`, `result`, `logs`, `files`, `continue`, `cancel`, `collect`, `ws init`, `capture`, `ui`, `skill install`, `setup`, `doctor`, `config`, `guide`.
- Web-UI-Backend und Headless-Backend, `--json`-Ausgabe und Exit-Codes wie im Original.
- Windows-Unterstützung (abgekoppelte Runner ohne Konsolenfenster, `taskkill`-Abbruch, `dsh.cmd`, Kopie statt Symlink als Fallback).
- Zusätzliche Programmdatei `go-dsh` neben dem kurzen Alias `dshgo`.

## Unveröffentlicht

- `ui stop [--force]` beendet die lokale DSH-Web-UI (Prozess über den Port gefunden, verweigert bei laufenden Web-Jobs).
- `ui start --open` öffnet den Browser; `ui` und `--open` verwenden die gemerkte Token-URL, damit der Browser angemeldet ist.
- Fehlermeldungen aus `dsh` (`reason.error.message`) werden angezeigt; leere Ergebnisse gelten als „keine Antwort“; `doctor` prüft den API-Schlüssel für Headless-Jobs.
- `setup` verknüpft standardmäßig keine Skills mehr (die feste Liste des Originals entfällt); mit `--skills a,b` wählen Sie Skills aus `~/.agents/skills`, nicht gefundene Namen werden gemeldet.
- `continue` akzeptiert auch einen Batch (neuer Batch mit den Fortsetzungen, `--only-failed`, `-p`).
- README auf Englisch (`README.md`) und Deutsch (`README.de.md`).

- Agent-Skill heißt `go-dsh-delegation` (statt `dsh-orchestration`).

- Profil heißt `concert` (statt `orchestra`), kuratierter Skill-Ordner `~/.dsh/skills-concert`.

- Umbenennung gegenüber dsh-cli: Alias `dshgo`, Zustandsordner `~/.dshgo`, Variablen `DSHGO_*`, Workspace-Marker `.dshgo-workspace`.
