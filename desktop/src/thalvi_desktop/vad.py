"""Owns local voice activity detection for the ending pause."""


class VoiceActivity:
    def __init__(self):
        try:
            import webrtcvad
        except ImportError as error:
            raise RuntimeError(
                "Install the desktop audio extra for voice activity detection."
            ) from error

        self.vad = webrtcvad.Vad(2)
        self.pending_vad = webrtcvad.Vad(3)

    def speech(self, frame: bytes, *, pending: bool = False) -> bool:
        if len(frame) != 7680:
            raise ValueError("Expected an 80 ms microphone frame.")
        vad = self.pending_vad if pending else self.vad
        return any([vad.is_speech(frame[i : i + 1920], 48_000) for i in range(0, 7680, 1920)])
