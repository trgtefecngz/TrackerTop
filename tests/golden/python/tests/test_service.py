from app.service import lookup, register


def test_register_admin():
    assert register("admin-1").name == "admin-1"


def test_lookup_missing():
    assert lookup("nobody") is None
