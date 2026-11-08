#!/usr/bin/env bash
# Read-only shape report of a whatsrust database, for sizing and checking the schema v9 migration
# (media download phase 2: the own-message repair and the identity forms it must match).
# It prints only counts and the shapes of identifiers (server, a '.' or ':' part, the length), never
# a JID, a phone number, a LID or message text, so its output is safe to paste into a review.
# It opens the database with `sqlite3 -readonly` and writes nothing.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/db-shape-report.sh <path/to/whatsapp.db>
       scripts/db-shape-report.sh --help

Prints a read-only report of a whatsrust database: file sizes, the schema version, the shape of the
account's own phone JID and LID (device row and LID mapping), message counts by source, the shapes
of stored sender and chat identifiers, and the number of rows the planned v9 repair would change.

Only counts and shapes are printed (for example "lid dot=1 colon=0 len=15"); never a JID, a number,
a LID or message text.

Safety:
  - The database is opened with `sqlite3 -readonly`; nothing is written.
  - Prefer a stopped daemon. If the daemon is running, report on a consistent copy instead:
      sqlite3 -readonly <db> ".backup /tmp/whatsrust-shape-copy.db"
      scripts/db-shape-report.sh /tmp/whatsrust-shape-copy.db
      rm /tmp/whatsrust-shape-copy.db
    (the copy contains your messages: delete it afterwards)

Example:
  scripts/db-shape-report.sh runs/dev-live/whatsapp.db
EOF
}

case "${1:-}" in
  -h|--help) usage; exit 0 ;;
  "") usage >&2; exit 2 ;;
esac

db="$1"
if [[ ! -f "$db" ]]; then
  echo "error: no such file: $db" >&2
  exit 2
fi
case "$db" in
  *-wal|*-shm) echo "error: pass the main database file, not its -wal/-shm file" >&2; exit 2 ;;
esac
command -v sqlite3 >/dev/null || { echo "error: sqlite3 not found" >&2; exit 2; }

size_of() { if [[ -f "$1" ]]; then wc -c <"$1" | tr -d ' '; else echo 0; fi; }
echo "== files (bytes)"
echo "db=$(size_of "$db") wal=$(size_of "$db-wal") shm=$(size_of "$db-shm")"

sqlite3 -readonly -bail -header -column "$db" <<'SQL'
.print
.print == schema
SELECT (SELECT user_version FROM pragma_user_version) AS user_version,
       (SELECT value FROM metadata WHERE key = 'schema_validated_version') AS validated_version;

-- Shape of an identifier: server class, a '.' or ':' in the user part, the length of the bare user.
-- user part = before '@'; bare user = user part before ':'.
CREATE TEMP VIEW own AS
  WITH d AS (SELECT pn, lid FROM device WHERE id = 1)
  SELECT
    CASE WHEN instr(pn, ':') > 0 THEN substr(pn, 1, instr(pn, ':') - 1)
         ELSE substr(pn, 1, instr(pn || '@', '@') - 1) END AS pn_user,
    CASE WHEN instr(lid, ':') > 0 THEN substr(lid, 1, instr(lid, ':') - 1)
         ELSE substr(lid, 1, instr(lid || '@', '@') - 1) END AS lid_user,
    pn, lid
  FROM d;

CREATE TEMP VIEW msg AS
  SELECT m.*,
    substr(sender_jid, 1, instr(sender_jid || '@', '@') - 1) AS s_user_part,
    CASE WHEN instr(substr(sender_jid, 1, instr(sender_jid || '@', '@') - 1), ':') > 0
         THEN substr(sender_jid, 1, instr(sender_jid, ':') - 1)
         ELSE substr(sender_jid, 1, instr(sender_jid || '@', '@') - 1) END AS s_bare,
    CASE WHEN sender_jid LIKE '%@lid' THEN 'lid'
         WHEN sender_jid LIKE '%@s.whatsapp.net' THEN 'pn'
         WHEN sender_jid LIKE '%@g.us' THEN 'group'
         WHEN instr(sender_jid, '@') = 0 THEN 'bare'
         ELSE 'other' END AS s_class,
    CASE WHEN chat_jid LIKE '%@lid' THEN 'lid'
         WHEN chat_jid LIKE '%@s.whatsapp.net' THEN 'pn'
         WHEN chat_jid LIKE '%@g.us' THEN 'group'
         WHEN instr(chat_jid, '@') = 0 THEN 'bare'
         ELSE 'other' END AS c_class
  FROM messages m;

.print
.print == own identity (shapes only)
SELECT
  CASE WHEN pn IS NULL OR pn = '' THEN 'none' WHEN pn LIKE '%@s.whatsapp.net' THEN 'pn' ELSE 'other' END AS pn_class,
  instr(pn_user, '.') > 0 AS pn_dot, instr(pn, ':') > 0 AS pn_colon, length(pn_user) AS pn_len,
  CASE WHEN lid IS NULL OR lid = '' THEN 'none' WHEN lid LIKE '%@lid' THEN 'lid' ELSE 'other' END AS lid_class,
  instr(lid_user, '.') > 0 AS lid_dot, instr(lid, ':') > 0 AS lid_colon, length(lid_user) AS lid_len
FROM own;
SELECT count(*) AS device_rows FROM device;

.print
.print == LID mapping rows for the own number (shapes only)
SELECT count(*) AS rows,
       sum(instr(lid, '.') > 0) AS with_dot,
       sum(lid = (SELECT lid_user FROM own)) AS equal_to_device_lid_user,
       sum(instr(lid, '.') > 0 AND substr(lid, 1, instr(lid, '.') - 1) = (SELECT lid_user FROM own)) AS dotted_twin_of_device_lid
FROM lid_pn_mapping WHERE phone_number = (SELECT pn_user FROM own);
SELECT count(*) AS mapping_rows_total, sum(instr(lid, '.') > 0) AS mapping_rows_with_dot FROM lid_pn_mapping;

.print
.print == messages by source
SELECT source, count(*) AS rows, sum(from_me) AS from_me_1,
       sum(content_kind = 'revoke') AS revokes
FROM messages GROUP BY source ORDER BY source;
SELECT count(*) AS media_refs_rows FROM media_refs;
SELECT count(*) AS backfill_cursor_rows,
       sum(oldest_msg_from_me IS NULL) AS cursor_flag_null,
       sum(oldest_msg_from_me = 0) AS cursor_flag_0
FROM backfill_cursor;

.print
.print == sender shapes (source, class, '.' in user, ':' in user)
SELECT source, s_class AS class, instr(s_bare, '.') > 0 AS dot, instr(s_user_part, ':') > 0 AS colon,
       count(*) AS rows
FROM msg GROUP BY 1, 2, 3, 4 ORDER BY 1, 2, 3, 4;

.print
.print == chat shapes (source, class)
SELECT source, c_class AS class, count(*) AS rows FROM msg GROUP BY 1, 2 ORDER BY 1, 2;

.print
.print == planned v9 repair sizes
-- live rows that the repair would mark as own (sender = own number or own LID, any form)
SELECT count(*) AS live_rows_to_mark_own
FROM msg, own
WHERE msg.source = 'live' AND msg.from_me = 0
  AND (msg.s_bare = own.pn_user OR msg.s_bare = own.lid_user
       OR (instr(msg.s_bare, '.') > 0 AND substr(msg.s_bare, 1, instr(msg.s_bare, '.') - 1) = own.lid_user)
       OR msg.s_bare IN (SELECT lid FROM lid_pn_mapping WHERE phone_number = own.pn_user));
-- history own rows whose sender is not the owner in any form (the repair would set the own phone JID)
SELECT count(*) AS history_own_rows_to_fix
FROM msg, own
WHERE msg.source IN ('bootstrap', 'backfill') AND msg.from_me = 1
  AND NOT (msg.s_bare = own.pn_user OR msg.s_bare = own.lid_user
           OR (instr(msg.s_bare, '.') > 0 AND substr(msg.s_bare, 1, instr(msg.s_bare, '.') - 1) = own.lid_user)
           OR msg.s_bare IN (SELECT lid FROM lid_pn_mapping WHERE phone_number = own.pn_user));
-- live rows filed under the own number as the chat (the self-chat plus own LID-chat rows filed there)
SELECT count(*) AS live_rows_in_own_number_chat, sum(from_me) AS of_which_from_me_1
FROM msg, own
WHERE msg.source = 'live' AND msg.chat_jid = own.pn_user || '@s.whatsapp.net';
SQL
