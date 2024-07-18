FROM python:3.11-slim-bullseye

WORKDIR /app
ENV PYTHONPATH=/app

COPY ronnia /app/ronnia

WORKDIR /app/ronnia

RUN pip install -r requirements.txt --no-cache-dir

COPY tests /tests

WORKDIR /app

ENTRYPOINT ["python", "ronnia/main.py"]