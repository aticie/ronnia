from itertools import islice


def convert_seconds_to_readable(seconds: str) -> str:
    seconds = int(seconds)
    minutes, seconds = divmod(seconds, 60)
    hours, minutes = divmod(minutes, 60)
    if hours == 0:
        return f"{minutes:g}:{seconds:02g}"
    else:
        return f"{hours:g}:{minutes:02g}:{seconds:02g}"


def batcher(iterable, batch_size):
    iterator = iter(iterable)
    while batch := list(islice(iterator, batch_size)):
        yield batch
