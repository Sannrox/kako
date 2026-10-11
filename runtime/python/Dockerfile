# Runtime image for the Python engine. Packages come from the hashed lockfile
# at build time. User code has no network at run time.
FROM python:3.12-slim

WORKDIR /opt/kako
COPY requirements.txt /opt/kako/requirements.txt
RUN pip install --no-cache-dir --require-hashes -r /opt/kako/requirements.txt \
    && python -c "import hashlib, pathlib; p = pathlib.Path('/opt/kako/requirements.txt'); pathlib.Path('/opt/kako/image-id').write_text('python-lockfile:sha256:' + hashlib.sha256(p.read_bytes()).hexdigest() + '\n')"
COPY kako_run.py /opt/kako/kako_run.py
ENV PYTHONNOUSERSITE=1 \
    PYTHONDONTWRITEBYTECODE=1 \
    KAKO_PYTHON=/usr/local/bin/python
