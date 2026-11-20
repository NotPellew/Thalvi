"""Chart view of ConfigSave.tla: who holds the config and how far that save got."""
import re

WRITERS = {'S': 'Settings save', 'A': 'agent settings change', 'L': 'login switch'}
LAST_STEP = {'S': 9, 'A': 9, 'L': 8}  # length of Flow(o) in the faithful, serialized design


def by_writer(value: str) -> dict[str, str]:
    return dict(re.findall(r'(\w+) \|-> (\w+)', value))


def state(variables: dict[str, str]) -> str:
    step = {o: int(n) for o, n in by_writer(variables['pc']).items()}
    failed = by_writer(variables['failed'])
    holder = variables['lock'].strip('"')
    if holder == 'nobody':
        if all(step[o] > LAST_STEP[o] or failed[o] == 'TRUE' for o in WRITERS):
            return 'every save returned [desktop.json, app and runner agree]'
        return 'no save in progress'
    who, at = WRITERS[holder], step[holder]
    if at <= 2:
        return f'{who} has the config to itself'
    if at <= 6:
        return f'{who} replacing desktop.json'
    if at == 7:
        return f'{who} replaced desktop.json'
    return f'{who} updated the running app'


ACTIONS = {
    'Lock': 'locks the config',
    'Snap': 'merges its change over the config',
    'RmTmp': None,
    'Create': None,
    'Write': None,
    'Rename': 'renames the new file over desktop.json',
    'Apply': 'sets the running config',
    'Send': None,
    'Unlock': 'unlocks',
    'Done': None,
}
