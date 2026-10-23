import os, base64, gzip

b64 = "H4sICGKf52gCA2luZGV4Lmh0bWwA7V1rc9tGsv4s/opeSao4BirvxCqpbJW3xnFsO5WyHZcrVamUIBISYcJBAJaifLffbj4wIChStpyqa9t7klSqYhg0mO7pHnRPD2b/95df/vP4+Ph4BsI+XMYwhHkM4RKG8wjCOIYQB4cQhvPZ7M9//vPBLPCpIQTz2eHhBARz8Z/Dg4MD8c+9u7u7+V2Kw/nhPAE1CL2bfn5P652n3nmpnaVSjEJax9O2G3mdT0/KjvRnVBo8asJ3U+S2U/1W/Fd+1r9WH0Uke2Z7J3uv/VN7s/VG+yf2W9qv2Gv2b7Z/ov2q/bX2r7Z/sP0T7Vftr2v/2vavtn+w/RPtr2v/2vavtn+w/RPtr2v/2t7+6uYIQRDDxY0L59HqnPmZzssGhAGGcB7BYgHD8BLC+a0XYrX64L12/VsHrC6yY3V+7byHa/UH72rn3av3s/OBdf7Ygb3rHczDQ3vv9O/TD9fWH7iv/uD9bf2B++oP3t/WH7j/qj94f1t/4P6r/uD9bf2B++oP3t/WH7j/qj94f1t/4L7qD95/52/J+IvwH+sfuK/+LX8vXr3/1/K3Jv9B2r8mfxvsv4o/Rf5RZP9e+jdi/y7+FuTfRf5V5N9F/lXk36X8q8i/i/yryL+L/KvIv4v8q8i/i/yryL+L/KvIv4v8q8i/i/yryL+L/KvIv4v8q8i/i/yryL+HzL8PCP8+IPz7gPDvA8K/Dwj/PiD8+4Dw7wPCvw8I/z4g/PuA8O8Dwr8PCK3+zQMIYRiGMOwfEBYaiiAMEToHhIU74E29A+zBbnv5B/Bjy1+tQ8h7HRE7//byj3V4oP27BuGC/VuHUALc8td/fvk5nENTQ6WEEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJpYQmlBKaUEpoQimhCaWEJpQSmlBKaEIpoQmlhCaUEppQSmhCKaEJPT09ffPmzfHxMQCAe+/f/w/DMNyH9v/z/9f9X/d/3f91/9f9X/d/3f91//8p7v+z2f8Dnj5VrQsEAAA="

compressed = base64.b64decode(b64)
html = gzip.decompress(compressed).decode('utf-8')

os.makedirs(r'C:\Users\Asus\terrafold', exist_ok=True)
with open(r'C:\Users\Asus\terrafold\index.html', 'w', encoding='utf-8') as f:
    f.write(html)
print(f'Wrote {len(html)} bytes to index.html')