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

    def speech(self, frame: bytes) -> bool:
        if len(frame) != 7680:
            raise ValueError("Expected an 80 ms microphone frame.")
        return any([self.vad.is_speech(frame[i : i + 1920], 48_000) for i in range(0, 7680, 1920)])
