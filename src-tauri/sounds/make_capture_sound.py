"""Synthesizes Screenshotr's capture sound: a crisp tick, a small glass "tink", and a soft
rising shimmer that suggests the shot going up. Run with numpy installed:
    python3 make_capture_sound.py capture.wav
"""
import sys
import wave

import numpy as np

RATE = 48_000
LENGTH = 0.42  # seconds
t = np.arange(int(RATE * LENGTH)) / RATE
rng = np.random.default_rng(7)  # fixed seed: the same sound every build


def envelope(start, attack, decay):
    """Starts at `start`, rises over `attack`, then decays exponentially with time constant `decay`."""
    local = t - start
    rise = np.clip(local / attack, 0, 1) if attack > 0 else (local >= 0).astype(float)
    return np.where(local >= 0, rise * np.exp(-np.maximum(local - attack, 0) / decay), 0.0)


def one_pole_highpass(signal, cutoff):
    alpha = 1 / (1 + 2 * np.pi * cutoff / RATE)
    out = np.zeros_like(signal)
    for i in range(1, len(signal)):
        out[i] = alpha * (out[i - 1] + signal[i] - signal[i - 1])
    return out


# 1. The tick: a few milliseconds of bright, filtered noise.
tick = one_pole_highpass(rng.standard_normal(len(t)), 2_500) * envelope(0.0, 0.0004, 0.0035) * 0.55

# 2. The tink: inharmonic partials, like a small glass bell, with the higher ones dying first.
base = 1_975.5  # B6
tink = np.zeros_like(t)
for ratio, level, decay in [(1.0, 0.42, 0.13), (2.756, 0.20, 0.07), (5.404, 0.09, 0.035), (8.933, 0.04, 0.018)]:
    tink += level * np.sin(2 * np.pi * base * ratio * t + ratio) * envelope(0.004, 0.0015, decay)

# 3. The shimmer: a quiet tone gliding up a fifth, fading in and out, with a light second voice.
start = 0.07
glide = np.clip((t - start) / 0.22, 0, 1)
frequency = 1_318.5 * (1.5 ** (glide ** 0.7))  # E6 up to B6
phase = 2 * np.pi * np.cumsum(frequency) / RATE
shimmer_env = envelope(start, 0.06, 0.11)
shimmer = (0.16 * np.sin(phase) + 0.05 * np.sin(2 * phase + 0.4)) * shimmer_env

signal = tick + tink + shimmer
# Fade the very end so the file finishes on silence, then normalize to -3 dBFS.
fade = np.clip((LENGTH - t) / 0.03, 0, 1)
signal *= fade
signal *= 10 ** (-3 / 20) / np.max(np.abs(signal))

with wave.open(sys.argv[1] if len(sys.argv) > 1 else "capture.wav", "wb") as out:
    out.setnchannels(1)
    out.setsampwidth(2)
    out.setframerate(RATE)
    out.writeframes((signal * 32_767).astype("<i2").tobytes())
