"""The audio projector: Whisper encoder frames in, Glimmer input embeddings out."""

import torch
import torch.nn.functional as F
from torch import nn


class AudioProjector(nn.Module):
    """Maps Whisper encoder frames to Glimmer input embeddings.

    Stacks `stack` consecutive frames into one vector, runs a SwiGLU MLP, and
    ends with the weightless RMSNorm Glimmer applies to its token and image
    embeddings, so audio embeddings arrive on the scale the decoder expects.
    """

    def __init__(
        self,
        encoder_dim: int = 1280,
        hidden_dim: int = 4096,
        text_dim: int = 6656,
        stack: int = 8,
        eps: float = 1e-5,
    ) -> None:
        super().__init__()
        self.stack = stack
        self.eps = eps
        self.in_norm = nn.RMSNorm(encoder_dim * stack, eps=eps)
        self.up = nn.Linear(encoder_dim * stack, 2 * hidden_dim, bias=False)
        self.down = nn.Linear(hidden_dim, text_dim, bias=False)

    def forward(self, frames: torch.Tensor) -> torch.Tensor:
        """Projects `(batch, frames, encoder_dim)` to `(batch, tokens, text_dim)`.

        A trailing partial stack is zero-padded, so there are
        `audio_token_count(frames)` tokens.
        """
        batch, length, dim = frames.shape
        frames = F.pad(frames, (0, 0, 0, (-length) % self.stack))
        stacked = frames.reshape(batch, -1, dim * self.stack)
        gate, value = self.up(self.in_norm(stacked)).chunk(2, dim=-1)
        out = self.down(F.silu(gate) * value)
        return F.rms_norm(out, (out.shape[-1],), eps=self.eps)

    def audio_token_count(self, frames: int) -> int:
        """Number of embeddings `forward` produces for `frames` encoder frames."""
        return -(-frames // self.stack)
