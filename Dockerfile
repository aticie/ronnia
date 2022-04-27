FROM python:3.9.10-slim

COPY ronnia /ronnia

WORKDIR /ronnia

RUN apt-get update && apt-get install -y build-essential libssl-dev uuid-dev cmake libcurl4-openssl-dev pkg-config -y
RUN pip install -r requirements.txt

COPY tests /tests

ENTRYPOINT ["python", "main.py"]