FROM python:3.9.10-slim

COPY src /src

WORKDIR /src

RUN pip install -r requirements.txt

COPY tests /tests

ENTRYPOINT ["python", "main.py"]