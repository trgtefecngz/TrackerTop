from app import audit, storage


class User:
    def __init__(self, name):
        self.name = name

    def validate(self):
        return bool(self.name)

    def save(self):
        if self.validate():
            storage.write(self.name)


class Admin(User):
    def validate(self):
        return self.name.startswith("admin")

    def save(self):
        super().save()
        audit.log("admin saved")
