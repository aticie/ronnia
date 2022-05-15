FROM python:3.9.10-slim

WORKDIR /app
ENV PYTHONPATH=/app

RUN apt-get update && apt-get install -y git build-essential libssl-dev uuid-dev cmake libcurl4-openssl-dev pkg-config -y

COPY ronnia /app/ronnia

WORKDIR /app/ronnia

RUN pip install -r requirements.txt

COPY tests /tests

WORKDIR /app

ENTRYPOINT ["python", "ronnia/main.py"]