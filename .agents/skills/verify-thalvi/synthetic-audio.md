# Synthetic audio verification

Use synthetic input and silent output for unattended verification. Physical
audio is not a routine plan item or completion requirement. Do not assume a
headset session or make buying a fixture a dependency.

For a specific issue or PR whose acceptance still needs physical audio, agents
may always ask the developer before merge. Finish the independent synthetic
checks first, explain exactly what they cannot establish, and propose a short
check. Arrange it only after the developer confirms a suitable setup and agrees.
Keep the acceptance gap explicit until the check is performed or the developer
resolves the requirement; synthetic evidence must not be relabelled as hardware
proof. Otherwise, describe hardware and subjective quality as outside scope.

## Coverage and evidence

| Layer | Drive | Required evidence |
| --- | --- | --- |
| Real recognition | Fixed generated speech through the isolated desktop microphone or Windows file feed | Model/build identity, fixture hash, transcript, destination/clipboard and cancellation; [desktop](desktop.md), [Windows](windows.md) |

Generated speech through real models establishes recognition on that corpus.
It does not establish physical microphone operation or perceived quality.
Paid model APIs are not part of this verification.
