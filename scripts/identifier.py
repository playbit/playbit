"""Canonical base58 text for opaque Playbit IDs in developer tools."""
ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'


def encode(raw):
    if not isinstance(raw, bytes) or not 1 <= len(raw) <= 32:
        raise ValueError('invalid identifier width')
    value = int.from_bytes(raw, 'big')
    text = ''
    while value:
        value, digit = divmod(value, 58)
        text = ALPHABET[digit] + text
    return '1' * (len(raw) - len(raw.lstrip(b'\0'))) + text


def decode(text, width=16):
    if not isinstance(text, str) or not 1 <= len(text) <= 44 or not 1 <= width <= 32:
        raise ValueError('invalid base58 identifier')
    value = 0
    for letter in text:
        value = value * 58 + ALPHABET.index(letter)
    raw = value.to_bytes(width, 'big')
    if encode(raw) != text:
        raise ValueError('noncanonical identifier width')
    return raw
