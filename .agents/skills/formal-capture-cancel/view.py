"""Chart view of CaptureCancel.tla: the stage of one capture and whether its text goes out."""


def state(variables: dict[str, str]) -> str:
    stage = variables['phase'].strip('"')
    if stage == 'done':
        return 'text goes out' if variables['delivered'] == 'TRUE' else 'nothing goes out'
    if stage == 'active':
        if variables['starting'] == 'TRUE':
            return 'listening, model still loading'
        return 'listening [a stop was seen]' if variables['cancelPending'] == 'TRUE' else 'listening'
    if stage == 'finish':
        return 'settling [a stop was seen]' if variables['ended'] == 'TRUE' else 'settling'
    return {
        'pre': 'reading controls queued before the capture',
        'cleaning': 'dictation cleanup before Enter',
        'ending': 'ending cue plays, recognizer releases',
        'preview': 'waiting for the full text on screen',
        'confirm': 'showing the text for approval',
        'writing': 'explicit write request',
    }[stage]


ACTIONS = {
    'Send': None,
    'DrainOne': 'reads a queued control',
    'PreEmpty': 'no controls left',
    'Receive': 'reads a control',
    'Loaded': 'model ready',
    'LoadFailed': 'load fails',
    'Prepare': 'Enter ending needs cleanup',
    'CleanupReturns': 'cleanup returns',
    'Accept': 'user ends the dictation',
    'ActiveCancelled': 'capture cancelled',
    'CueReturns': None,
    'CloseReturns': 'recognizer released',
    'PreviewReturns': 'preview is shown or unavailable',
    'ConfirmReturns': 'confirmation ends',
    'WritingReturns': 'write request returns',
    'FinishEmpty': 'no controls left',
}
