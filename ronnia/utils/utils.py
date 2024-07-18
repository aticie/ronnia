from typing import AsyncIterable, AsyncGenerator


def convert_seconds_to_readable(seconds: str) -> str:
    seconds = int(seconds)
    minutes, seconds = divmod(seconds, 60)
    hours, minutes = divmod(minutes, 60)
    if hours == 0:
        return f"{minutes:g}:{seconds:02g}"
    else:
        return f"{hours:g}:{minutes:02g}:{seconds:02g}"


async def async_batcher(iterable: AsyncIterable, batch_size: int) -> AsyncGenerator:
    batch = []
    async for item in iterable:
        batch.append(item)
        if len(batch) == batch_size:
            yield batch
            batch = []

    yield batch
