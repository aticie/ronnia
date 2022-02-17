FROM python:3.9.0-slim

COPY src /src

WORKDIR /src

RUN pip install -r requirements.txt

COPY tests /tests

ENTRYPOINT ["python", "main.py"]