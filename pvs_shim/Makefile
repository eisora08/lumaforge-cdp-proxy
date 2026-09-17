CC ?= gcc
CFLAGS = -O2 -Wall -Wextra -Wno-unused-parameter
TARGET = pvs_shim

all: $(TARGET)

$(TARGET): pvs_shim.c
	$(CC) $(CFLAGS) -o $@ $<

install: $(TARGET)
	install -D -m 755 $(TARGET) /usr/local/bin/$(TARGET)

clean:
	rm -f $(TARGET)

.PHONY: all install clean
