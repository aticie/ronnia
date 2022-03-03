FROM python:3.9.10-slim

COPY ronnia /ronnia

WORKDIR /ronnia

RUN pip install -r requirements.txt

COPY tests /tests

ENTRYPOINT ["python", "main.py"]