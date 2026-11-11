# Custom fields

Custom fields are the structured facts an MSP keeps about its devices and
companies beyond their notes and tags: a contract tier, a backup vendor,
the date of the last disaster recovery test, an office location, a local
administrator password. Administrators define the fields; technicians fill
them in on the website; [toolbox scripts](toolbox.md) read them, and write
the ones they are allowed to, through the Agent.

## Fields

**Companies → Custom fields** (`/custom-fields`) lists the fields in the
order every page shows them. Users with `custom_fields.manage` create,
change, reorder and delete them. A field has:

- a **label**, up to 80 characters, and an optional **description**, shown
  under it;
- a **key**, the stable name scripts and the API know it by: a lowercase
  letter, then up to 63 lowercase letters, digits and underscores
  (`contract_tier`). The website suggests one from the label. It can't
  change once the field exists;
- a **type**, which can't change either:

  | Type | Holds | Stored and given to scripts as |
  |---|---|---|
  | Text | One line, up to 1,000 characters | The text, trimmed |
  | Multi-line text | Up to 10,000 characters | The text, with `\n` line ends |
  | Number | Any finite number | Decimal, without an exponent: `12`, `-1.5` |
  | Checkbox | Yes or no | `true` or `false` |
  | Date | A calendar date | `YYYY-MM-DD` |
  | Dropdown | One of its options | The option |
  | Multi-select | Some of its options | The options, in the field's order, one per line |
  | Web address | An `http://` or `https://` address, up to 2,048 characters | The address |
  | Secret | Up to 4,096 characters, kept encrypted | The text, as it was typed |

  A dropdown or multi-select has 1 to 100 options of up to 100
  characters each, unique ignoring case;
- where it **applies**: devices, companies, or both;
- what **scripts** may do with it: nothing (the default), read it, or read
  and write it.

An install has at most 200 fields. Changing where a field applies removes
the values left without a place (companies' values of a field that now
applies to devices only, say), and removing or renaming an option takes it
out of the values that held it; the audit log says how many values went.
Deleting a field deletes every value of it.

## Values

A device's page and a company's page (`/company?id=…`, opened from its name
on the **Companies** page) have a **Fields** section listing every field
that applies to it, edited in place with an input for its type: a number
box, a date picker, a list of options, checkboxes for a multi-select.
Each value shows who last changed it and when, either a user or the script
that set it ("Changed by the script “Backup check” 3 minutes ago").

- Anyone with `devices.view` reads devices' and companies' values.
- **`devices.edit`** sets and clears devices' values, the permission that
  already edits their tags and notes, so the Technician role has it.
  Values are about the device as much as its tags are, and a separate
  permission would only be granted alongside it.
- **`companies.manage`** sets and clears companies' values. A company's
  value reaches all of its devices, so it takes the permission that
  creates and renames companies.

The server checks every value against its field's type, whoever sets it:
the website, the API or a script. An empty value (an empty text, no option,
`null`) clears it.

### Inheritance

A field that applies to both devices and companies lets a company's value
stand for its devices: a device without a value of its own shows its
company's, marked "From the company Acme", and scripts on it read the
company's. Setting the device's own value overrides it; clearing that
(**Use the company's**) goes back to the company's. A device moved to
another company takes that company's values; one in no company has only
its own.

### Secrets

A secret field's values are encrypted at rest with the instance key
(AES-256-GCM, bound to the field, and to the device or company, so a value
copied to another row doesn't decrypt), like the other secrets the server
keeps (see `server/src/secrets.rs`). The website shows a secret as dots.
No list carries it: not a device's or company's fields, not the device
list, not the audit log.

**Reveal** shows one value. It needs **`custom_fields.reveal`**, which no
role but Administrator has until it is granted, and every reveal is
recorded in the audit log (`custom_field.reveal`, naming the field and the
device or company, never the value). Setting a secret needs only
`devices.edit` or `companies.manage`: the website asks for a new value and
never shows the old one. Scripts allowed to read a secret field get its
value in plain text, like any other.

### Many devices at once

**Set field** in the device wall's selection bar sets one field on the
chosen devices, or clears it (up to 1,000 at a time), with `devices.edit`.
Each device's change is recorded in the audit log.

### Filtering the device wall

The wall's **Any field** menu shows the devices whose field matches: **is**
a value (text ignoring case, a number, a date, an option, yes or no),
**contains** some text (or, for a multi-select, has an option), **has a
value** or **is empty**. A device's company's value counts for it. A secret
can only be filtered by whether it has a value. The filter is kept in the
page's address (`?field=<key>&field_op=equals&field_value=Gold`), and the
server works out which devices match.

### Audit

`custom_field.create`, `.update`, `.delete` and `.reorder` record changes
to the fields. `custom_field.set` records each value set or cleared on a
device or company, with the field, the value (cut to 200 characters, and
`(secret)` for a secret) and where the change came from: `website`, `bulk`
or `script`. A script's change is recorded as `script “<name>”`, with the
run's ID and the user who started it. `custom_field.reveal` records each
reveal.

## Scripts

When the Agent runs a toolbox script, from the website, the viewer, a
[job](automation.md), a policy or an alert rule's self-healing, the server
sends the device's fields that scripts may read along with it, each with its
value (the device's own, or its company's). Values are read when each run
starts, so a job on many devices sends each its own. Monitors' script
checks get none.

### Reading

Each readable field with a value is an environment variable named
`MESHRMM_FIELD_` and its key in capitals: `contract_tier` is
`MESHRMM_FIELD_CONTRACT_TIER`. A field without a value has no variable. The
value is the text form in the table above: a multi-select's options one per
line, a checkbox `true` or `false`.

The script also gets `MESHRMM_RUN_ID`, its run's ID, and `MESHRMM_AGENT`, the
Agent's executable, whose folder is added to the end of `PATH` so
`meshrmm-agent` runs by name.

### Writing

A script sets a field it may write with the Agent's command:

```text
meshrmm-agent field set <key> <value>      set it; several values for a multi-select's options
meshrmm-agent field set <key> --stdin      the value from standard input, for long or multi-line text
meshrmm-agent field clear <key>            clear it
meshrmm-agent field get <key>              print the value the script started with
```

It exits with 0 once the server has the value, and otherwise prints why to
standard error and exits with 1 (2 for a mistake in the command), so a
script can check. The value appears on the device's page at once, recorded
as the script's change. A write changes the device's own value, never its
company's: scripts can't write company-only fields.

How it works: when the script may write any field, the Agent gives the run
a random token for a channel it keeps open on `127.0.0.1` for this, in
`MESHRMM_FIELDS_ENDPOINT` and `MESHRMM_FIELDS_TOKEN`. `meshrmm-agent field
set` sends the token, key and value there. The Agent takes the token only
while that run's script is running, refuses fields the run may not write,
and forwards the value to the server as that run, with its own credential
(`POST /v1/agents/{id}/script-runs/{run_id}/custom-fields`). The server
checks that the run belongs to the device and hasn't finished, that the
field applies to devices and scripts may write it, and the value's type,
before it stores it with the run and the script's name. The token is
useless once the script exits, and works only for that run's device and
writable fields; nothing a script runs can write another device's fields,
or fields an administrator didn't open to scripts.

On Windows the variables are in the script's environment block. On macOS
and Linux they are exported by lines the Agent puts at the top of the
script file, which only the account the script runs as can read (`sudo`,
which starts a Mac script as the signed-in user, drops a process's
environment), so line numbers in a shell script's error messages count
those lines too. A value can be read by the account the script runs as,
and by administrators or root, while the script runs: other processes of
that account can read its environment, as they can its files.

### Examples

PowerShell:

```powershell
$tier = $env:MESHRMM_FIELD_CONTRACT_TIER
if ($tier -eq 'Gold') { Write-Output 'Gold customer: keeping 90 days of logs' }

$last = (Get-WinEvent -FilterHashtable @{ LogName = 'Application'; ProviderName = 'Veeam*' } -MaxEvents 1).TimeCreated
meshrmm-agent field set last_backup $last.ToString('yyyy-MM-dd')
if ($LASTEXITCODE -ne 0) { exit 1 }

# A multi-select's options are lines.
$services = $env:MESHRMM_FIELD_SERVICES -split "`n"
meshrmm-agent field set services Backup Antivirus
```

Command Prompt:

```bat
@echo off
if "%MESHRMM_FIELD_CONTRACT_TIER%"=="Gold" echo Gold customer
for /f "tokens=*" %%v in ('ver') do meshrmm-agent field set windows_build "%%v"
if errorlevel 1 exit /b 1
systeminfo | meshrmm-agent field set system_summary --stdin
```

zsh on macOS and bash on Linux:

```sh
echo "Backup vendor: ${MESHRMM_FIELD_BACKUP_VENDOR:-none set}"
meshrmm-agent field set disk_encrypted "$(fdesetup isactive 2>/dev/null || echo false)"   # macOS
meshrmm-agent field set kernel "$(uname -r)" || exit 1
printf '%s\n' "$MESHRMM_FIELD_SERVICES" | while IFS= read -r service; do echo "has $service"; done
meshrmm-agent field clear pending_reboot
```

## API

See [the API](api.md#custom-fields) for the routes.

## How it's built

- `server/src/custom_fields.rs`: types, the canonical form of each type's
  values and their checks, sealing secrets, inheritance, the filter's
  comparison and the fields a run gets. `server/src/api/custom_fields.rs`:
  the routes; every one about a device or company finds it through
  `authorized_device` or `authorized_company`.
- Tables `custom_fields`, `device_field_values` and `company_field_values`.
  A secret's row holds `value_encrypted` and no `value`.
- `crates/protocol-types`: `ScriptRunRequest.fields` and
  `ScriptFieldWrite`.
- `agent/src/script_fields.rs`: the environment, the local channel and its
  tokens, forwarding to the server, and the `meshrmm-agent field` command.
  `agent/src/remote/toolbox/` passes the variables to the interpreter.
- `dashboard/features/custom-fields/`: the fields page, the **Fields**
  section, the wall's filter and **Set field**.

## Tests

- `cargo test -p meshrmm-server --test custom_fields`, on SQLite and
  PostgreSQL: definitions and who may change them, each type's checks,
  inheritance, secrets sealed in the database, masked everywhere and
  revealed only with the permission and on the record, the filter, bulk
  changes, and scripts' reads and writes, the ones refused included.
- `cargo test -p meshrmm-agent script_fields` and
  `remote::toolbox::windows`: the variables, the command, the channel's
  tokens and refusals, the shell exports read back by bash and zsh, and the
  Windows environment block.
- `npm test` in `dashboard`: the values' checks per type, the filter's
  address and the wall's filtering.
