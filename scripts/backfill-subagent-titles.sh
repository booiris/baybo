#!/usr/bin/env bash
#
# One-off: give existing subagent child sessions the errand that spawned them.
#
# `spawn_subagent` now stamps `task_summary` onto the child's `title` at spawn
# (see `resolve_child_session`), because that is the only place the text ever
# reaches the session row — it otherwise lives solely in the spawn request and
# the trace span. Children minted BEFORE that landed have `title IS NULL`, so
# the iOS subagent list can only fall back to their profile name and shows a
# wall of identical "general-purpose" rows.
#
# The text is recovered from the tool-call span the child already points at
# (`sessions.parent_span_id` → `spans.data -> $.kind.begin.params.description`),
# which is the same string the spawn path writes today.
#
# Safe to re-run: it only ever fills a NULL, exactly like the
# `set_title_if_absent` setter it stands in for, and it writes the flat `title`
# column only — the same column the production setters write, and the one `get`
# treats as authoritative over the JSON blob.
#
#   scripts/backfill-subagent-titles.sh [--apply] [path/to/storage.db]
#
# Without `--apply` it reports what it would change and writes nothing.
set -euo pipefail

APPLY=0
DB="${HOME}/.baybo/state/storage.db"
for arg in "$@"; do
    case "$arg" in
        --apply) APPLY=1 ;;
        *) DB="$arg" ;;
    esac
done

if [[ ! -f "$DB" ]]; then
    echo "no such database: $DB" >&2
    exit 1
fi

# The candidate set, spelled once: a subagent child with no title whose spawn
# span carries a non-empty description.
WHERE="lineage_kind IS NOT NULL
       AND title IS NULL
       AND EXISTS (SELECT 1 FROM spans s
                    WHERE s.id = sessions.parent_span_id
                      AND json_extract(s.data,'\$.kind.begin.tool_name') = 'spawn_subagent'
                      AND trim(coalesce(json_extract(s.data,'\$.kind.begin.params.description'),'')) <> '')"

count=$(sqlite3 -readonly "$DB" "SELECT COUNT(*) FROM sessions WHERE $WHERE;")
echo "children to title: $count"

if [[ "$count" == "0" ]]; then
    echo "nothing to do"
    exit 0
fi

echo
echo "sample (profile → title):"
sqlite3 -readonly -column "$DB" "
  SELECT substr(json_extract(c.data,'\$.state.subagent_type'),1,18),
         substr(json_extract(s.data,'\$.kind.begin.params.description'),1,60)
    FROM sessions c JOIN spans s ON s.id = c.parent_span_id
   WHERE c.lineage_kind IS NOT NULL AND c.title IS NULL
   ORDER BY c.created_at DESC LIMIT 5;"

if [[ "$APPLY" != "1" ]]; then
    echo
    echo "dry run — pass --apply to write"
    exit 0
fi

# The exact undo. Only NULLs are filled, so restoring means nulling precisely
# these ids — captured BEFORE the write, since the predicate stops matching
# them the moment it lands.
undo_dir="$(dirname "$DB")/../../.baybo-migrations"
mkdir -p "$undo_dir"
undo="$undo_dir/subagent-title-backfill-$(date -u +%Y%m%dT%H%M%SZ).ids"
sqlite3 -readonly "$DB" "SELECT id FROM sessions WHERE $WHERE;" > "$undo"
echo
echo "undo list: $undo ($(wc -l < "$undo" | tr -d ' ') ids)"
echo "  to revert: sqlite3 \"$DB\" \"UPDATE sessions SET title = NULL WHERE id IN (…ids…);\""

sqlite3 "$DB" "
  UPDATE sessions
     SET title = (SELECT json_extract(s.data,'\$.kind.begin.params.description')
                    FROM spans s WHERE s.id = sessions.parent_span_id)
   WHERE $WHERE;"

remaining=$(sqlite3 -readonly "$DB" "SELECT COUNT(*) FROM sessions WHERE $WHERE;")
echo "done — remaining untitled: $remaining"
