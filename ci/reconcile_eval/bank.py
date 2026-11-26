"""Phrase bank for the E15 synthetic evaluation set (fictional people only, no real data).

A *slot* is a single-valued state of a person (home city, diet, ...). Each value has
paraphrase variants in English and Russian: full sentence templates with placeholders

  {S}  subject at sentence start (full name / first name / pronoun)
  {s}  the same in mid-sentence (pronoun lower-cased)
  {P}  English possessive at sentence start ("Alice's" / "Her")
  {G}  Russian genitive after a preposition ("Анны Ветровой" / "неё")
  [m|f]  gender alternatives (Russian agreement, English his/her)

Two values of one slot exclude each other (as far as a careful reader would judge); two
variants of one value say the same thing (a duplicate). Groups put slots on related topics
together; a slot of the same group is the "hard negative" (compatible, same topic).
"""

# ---- people -------------------------------------------------------------------------------

EN_PEOPLE = [
    ("Alice Thornbury", "f"), ("Marcus Delaney", "m"), ("Priya Holloway", "f"),
    ("Owen Castellan", "m"), ("Greta Lindqvist", "f"), ("Tobias Wren", "m"),
    ("Nadia Fairweather", "f"), ("Elliot Marsh", "m"), ("Hazel Okafor", "f"),
    ("Rupert Vance", "m"), ("Ingrid Halvorsen", "f"), ("Dominic Ashby", "m"),
    ("Leona Pritchard", "f"), ("Felix Ambrose", "m"), ("Clara Weston", "f"),
    ("Jasper Quill", "m"), ("Maeve Donnelly", "f"), ("Silas Harrow", "m"),
    ("Rosa Kettering", "f"), ("Adrian Pell", "m"), ("Beatrix Colvin", "f"),
    ("Hugo Brannigan", "m"), ("Iris Mallory", "f"), ("Callum Strand", "m"),
]

# (full nominative, full genitive, first nominative, first genitive, gender)
RU_PEOPLE = [
    ("Анна Ветрова", "Анны Ветровой", "Анна", "Анны", "f"),
    ("Олег Сомов", "Олега Сомова", "Олег", "Олега", "m"),
    ("Марина Крылова", "Марины Крыловой", "Марина", "Марины", "f"),
    ("Игорь Белов", "Игоря Белова", "Игорь", "Игоря", "m"),
    ("Светлана Озерова", "Светланы Озеровой", "Светлана", "Светланы", "f"),
    ("Дмитрий Ласточкин", "Дмитрия Ласточкина", "Дмитрий", "Дмитрия", "m"),
    ("Юлия Соболева", "Юлии Соболевой", "Юлия", "Юлии", "f"),
    ("Павел Гречкин", "Павла Гречкина", "Павел", "Павла", "m"),
    ("Вера Миронова", "Веры Мироновой", "Вера", "Веры", "f"),
    ("Артём Зимин", "Артёма Зимина", "Артём", "Артёма", "m"),
    ("Ксения Ладыгина", "Ксении Ладыгиной", "Ксения", "Ксении", "f"),
    ("Егор Румянцев", "Егора Румянцева", "Егор", "Егора", "m"),
    ("Полина Чайкина", "Полины Чайкиной", "Полина", "Полины", "f"),
    ("Никита Воронцов", "Никиты Воронцова", "Никита", "Никиты", "m"),
    ("Дарья Снегирёва", "Дарьи Снегирёвой", "Дарья", "Дарьи", "f"),
    ("Роман Кедров", "Романа Кедрова", "Роман", "Романа", "m"),
    ("Алёна Брусникина", "Алёны Брусникиной", "Алёна", "Алёны", "f"),
    ("Степан Журавлёв", "Степана Журавлёва", "Степан", "Степана", "m"),
    ("Ольга Тимофеева", "Ольги Тимофеевой", "Ольга", "Ольги", "f"),
    ("Лев Сорокин", "Льва Сорокина", "Лев", "Льва", "m"),
    ("Евгения Пташкина", "Евгении Пташкиной", "Евгения", "Евгении", "f"),
    ("Тарас Голубев", "Тараса Голубева", "Тарас", "Тараса", "m"),
    ("Лиана Аксёнова", "Лианы Аксёновой", "Лиана", "Лианы", "f"),
    ("Вадим Стрельцов", "Вадима Стрельцова", "Вадим", "Вадима", "m"),
]

# mixed: Cyrillic forms as RU_PEOPLE plus the Latin spelling an English note would use;
# canonical = which spelling the garden stores as the entity name.
MIXED_PEOPLE = [
    ("Мирослава Кузнецова", "Мирославы Кузнецовой", "Мирослава", "Мирославы", "f", "Miroslava Kuznetsova", "cyr"),
    ("Тимур Алиев", "Тимура Алиева", "Тимур", "Тимура", "m", "Timur Aliev", "lat"),
    ("Ника Гордеева", "Ники Гордеевой", "Ника", "Ники", "f", "Nika Gordeeva", "cyr"),
    ("Глеб Савельев", "Глеба Савельева", "Глеб", "Глеба", "m", "Gleb Saveliev", "lat"),
    ("Злата Ефимова", "Златы Ефимовой", "Злата", "Златы", "f", "Zlata Efimova", "cyr"),
    ("Марк Лебедев", "Марка Лебедева", "Марк", "Марка", "m", "Mark Lebedev", "lat"),
    ("Ева Кравец", "Евы Кравец", "Ева", "Евы", "f", "Eva Kravets", "cyr"),
    ("Остап Мельник", "Остапа Мельника", "Остап", "Остапа", "m", "Ostap Melnyk", "lat"),
    ("Лада Петрова", "Лады Петровой", "Лада", "Лады", "f", "Lada Petrova", "cyr"),
    ("Фёдор Гусев", "Фёдора Гусева", "Фёдор", "Фёдора", "m", "Fyodor Gusev", "lat"),
    ("Ирина Белая", "Ирины Белой", "Ирина", "Ирины", "f", "Irina Belaya", "cyr"),
    ("Савва Морозов", "Саввы Морозова", "Савва", "Саввы", "m", "Savva Morozov", "lat"),
    ("Таисия Орлова", "Таисии Орловой", "Таисия", "Таисии", "f", "Taisia Orlova", "cyr"),
    ("Кирилл Ушаков", "Кирилла Ушакова", "Кирилл", "Кирилла", "m", "Kirill Ushakov", "lat"),
    ("Мила Добрынина", "Милы Добрыниной", "Мила", "Милы", "f", "Mila Dobrynina", "cyr"),
    ("Ян Ковальский", "Яна Ковальского", "Ян", "Яна", "m", "Yan Kovalsky", "lat"),
    ("Софья Рябова", "Софьи Рябовой", "Софья", "Софьи", "f", "Sofya Ryabova", "cyr"),
    ("Борис Нечаев", "Бориса Нечаева", "Борис", "Бориса", "m", "Boris Nechaev", "lat"),
    ("Агата Левина", "Агаты Левиной", "Агата", "Агаты", "f", "Agata Levina", "cyr"),
    ("Данияр Сеитов", "Данияра Сеитова", "Данияр", "Данияра", "m", "Daniyar Seitov", "lat"),
    ("Элина Шубина", "Элины Шубиной", "Элина", "Элины", "f", "Elina Shubina", "cyr"),
    ("Арсений Котов", "Арсения Котова", "Арсений", "Арсения", "m", "Arseny Kotov", "lat"),
    ("Вероника Сазонова", "Вероники Сазоновой", "Вероника", "Вероники", "f", "Veronika Sazonova", "cyr"),
    ("Илья Брагин", "Ильи Брагина", "Илья", "Ильи", "m", "Ilya Bragin", "lat"),
]

# ---- calendar -----------------------------------------------------------------------------

MONTHS_EN = ["January", "February", "March", "April", "May", "June", "July", "August",
             "September", "October", "November", "December"]
MONTHS_RU_PREP = ["январе", "феврале", "марте", "апреле", "мае", "июне", "июле", "августе",
                  "сентябре", "октябре", "ноябре", "декабре"]
MONTHS_RU_GEN = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа",
                 "сентября", "октября", "ноября", "декабря"]

# en, ru nominative, ru prepositional, ru accusative
CITIES = [
    ("Lisbon", "Лиссабон", "Лиссабоне", "Лиссабон"), ("Riga", "Рига", "Риге", "Ригу"),
    ("Tbilisi", "Тбилиси", "Тбилиси", "Тбилиси"), ("Belgrade", "Белград", "Белграде", "Белград"),
    ("Valencia", "Валенсия", "Валенсии", "Валенсию"), ("Kazan", "Казань", "Казани", "Казань"),
    ("Porto", "Порту", "Порту", "Порту"), ("Yerevan", "Ереван", "Ереване", "Ереван"),
    ("Vienna", "Вена", "Вене", "Вену"), ("Krakow", "Краков", "Кракове", "Краков"),
    ("Istanbul", "Стамбул", "Стамбуле", "Стамбул"), ("Helsinki", "Хельсинки", "Хельсинки", "Хельсинки"),
]


def _city_values(idxs, en_tpls, ru_tpls):
    out = []
    for i in idxs:
        en, rn, rp, ra = CITIES[i]
        out.append({
            "en": [t.format(c=en, S="{S}", s="{s}", P="{P}") for t in en_tpls],
            "ru": [t.format(c=rn, cp=rp, ca=ra, S="{S}", s="{s}", G="{G}") for t in ru_tpls],
        })
    return out


# ---- slots --------------------------------------------------------------------------------
# id -> {"group", "values": [{"en": [...], "ru": [...]}], "change": bool}

SLOTS = {}


def slot(name, group, values, change=True):
    SLOTS[name] = {"group": group, "values": values, "change": change}


slot("home_city", "home", _city_values(
    [0, 1, 2, 3, 4, 5],
    ["{S} lives in {c}.", "{S} has made {c} [his|her] home.", "{P} home is in {c}."],
    ["{S} живёт в {cp}.", "{S} [обосновался|обосновалась] в {cp}.", "Дом у {G} в {cp}."]))
slot("parents_city", "family", _city_values(
    [6, 7, 8, 9, 1, 5],
    ["{P} parents live in {c}.", "{S} has [his|her] parents in {c}.", "{P} mum and dad are in {c}."],
    ["Родители {G} живут в {cp}.", "{S} ездит к родителям в {ca}, они там живут.", "У {G} родители в {cp}."]),
    change=False)
slot("housing", "home", [
    {"en": ["{S} rents a one-bedroom flat.", "{S} lives in a rented apartment with one bedroom."],
     "ru": ["{S} снимает однокомнатную квартиру.", "{S} живёт в съёмной однушке."]},
    {"en": ["{S} owns a house with a small garden.", "{S} lives in [his|her] own house with a garden."],
     "ru": ["{S} владеет собственным домом с садом.", "{S} живёт в своём доме, при доме есть сад."]},
    {"en": ["{S} shares a flat with two friends.", "{S} lives with two flatmates."],
     "ru": ["{S} снимает квартиру вместе с двумя друзьями.", "{S} живёт с двумя соседями по квартире."]},
    {"en": ["{S} still lives with [his|her] parents.", "{S} lives at [his|her] parents' place."],
     "ru": ["{S} до сих пор живёт с родителями.", "{S} живёт у родителей."]},
])
slot("commute", "transport", [
    {"en": ["{S} cycles to work.", "{S} gets to the office by bike."],
     "ru": ["{S} ездит на работу на велосипеде.", "{S} добирается до офиса на велике."]},
    {"en": ["{S} takes the metro to work.", "{S} commutes by underground."],
     "ru": ["{S} ездит на работу на метро.", "{S} добирается до работы на подземке."]},
    {"en": ["{S} drives to work.", "{S} commutes to the office by car."],
     "ru": ["{S} ездит на работу на машине.", "{S} добирается до работы за рулём."]},
    {"en": ["{S} walks to work.", "{S} goes to the office on foot."],
     "ru": ["{S} ходит на работу пешком.", "{S} добирается до офиса пешком."]},
])
slot("car", "transport", [
    {"en": ["{S} drives an old Volvo estate.", "{P} car is a battered Volvo estate."],
     "ru": ["{S} ездит на старом универсале Volvo.", "Машина у {G} — потрёпанный универсал Volvo."]},
    {"en": ["{S} drives a red Kia Rio.", "{P} car is a red Kia Rio."],
     "ru": ["{S} ездит на красной Kia Rio.", "У {G} красная Kia Rio."]},
    {"en": ["{S} doesn't own a car.", "{S} has no car of [his|her] own."],
     "ru": ["У {G} нет машины.", "{S} не владеет автомобилем."]},
    {"en": ["{S} drives an electric Nissan Leaf.", "{P} car is an electric Nissan Leaf."],
     "ru": ["{S} ездит на электрическом Nissan Leaf.", "У {G} электромобиль Nissan Leaf."]},
])
slot("employer", "work", [
    {"en": ["{S} works at Northwind Logistics.", "{S} is employed by Northwind Logistics."],
     "ru": ["{S} работает в «Нордвинд Логистик».", "{S} [устроен|устроена] в компанию «Нордвинд Логистик»."]},
    {"en": ["{S} works at Sapfir Bank.", "{P} employer is Sapfir Bank."],
     "ru": ["{S} работает в банке «Сапфир».", "Место работы {G} — банк «Сапфир»."]},
    {"en": ["{S} works at the city hospital.", "{S} is on the staff of the city hospital."],
     "ru": ["{S} работает в городской больнице.", "{S} в штате городской больницы."]},
    {"en": ["{S} works for Kestrel Robotics.", "{P} employer is Kestrel Robotics, a robotics start-up."],
     "ru": ["{S} работает в стартапе Kestrel Robotics.", "{S} трудится в робототехнической компании Kestrel Robotics."]},
])
slot("job_title", "work", [
    {"en": ["{S} is a software developer.", "{S} writes software for a living."],
     "ru": ["{S} — программист.", "{S} работает разработчиком программ."]},
    {"en": ["{S} is a school teacher.", "{S} teaches at a secondary school."],
     "ru": ["{S} — школьный учитель.", "{S} преподаёт в средней школе."]},
    {"en": ["{S} is an accountant.", "{S} works as an accountant."],
     "ru": ["{S} — бухгалтер.", "{S} работает бухгалтером."]},
    {"en": ["{S} is an architect.", "{S} designs buildings as an architect."],
     "ru": ["{S} — архитектор.", "{S} работает архитектором, проектирует здания."]},
])
slot("work_schedule", "work", [
    {"en": ["{S} works fully remotely.", "{S} works from home all week."],
     "ru": ["{S} работает полностью удалённо.", "{S} всю неделю работает из дома."]},
    {"en": ["{S} works night shifts.", "{S} is on the night shift at work."],
     "ru": ["{S} работает в ночные смены.", "{S} выходит на работу по ночам, сменами."]},
    {"en": ["{S} works a four-day week.", "{S} has Fridays off and works four days a week."],
     "ru": ["{S} работает четыре дня в неделю.", "У {G} четырёхдневная рабочая неделя, пятница выходной."]},
    {"en": ["{S} goes to the office every weekday.", "{S} works in the office five days a week."],
     "ru": ["{S} каждый будний день ходит в офис.", "{S} пять дней в неделю работает в офисе."]},
])
slot("diet", "food", [
    {"en": ["{S} is vegetarian.", "{S} doesn't eat meat at all."],
     "ru": ["{S} — [вегетарианец|вегетарианка].", "{S} совсем не ест мясо."]},
    {"en": ["{S} is vegan.", "{S} eats no animal products whatsoever."],
     "ru": ["{S} — [веган|веганка].", "{S} не ест никаких продуктов животного происхождения."]},
    {"en": ["{S} eats meat every day.", "{S} loves steak and eats meat often."],
     "ru": ["{S} ест мясо каждый день.", "{S} обожает стейки и часто ест мясо."]},
    {"en": ["{S} eats fish but no other meat.", "{S} is a pescatarian."],
     "ru": ["{S} ест рыбу, но не ест мясо.", "{S} — [пескетарианец|пескетарианка]."]},
])
slot("coffee", "food", [
    {"en": ["{S} drinks coffee black, no sugar.", "{S} takes coffee without milk or sugar."],
     "ru": ["{S} пьёт чёрный кофе без сахара.", "{S} пьёт кофе без молока и сахара."]},
    {"en": ["{S} always orders an oat-milk latte.", "{S} drinks coffee only with oat milk."],
     "ru": ["{S} всегда берёт латте на овсяном молоке.", "{S} пьёт кофе только с овсяным молоком."]},
    {"en": ["{S} doesn't drink coffee, only tea.", "{S} prefers green tea and doesn't touch coffee."],
     "ru": ["{S} не пьёт кофе, только чай.", "{S} предпочитает зелёный чай, кофе не пьёт."]},
    {"en": ["{S} drinks only decaf.", "{S} sticks to decaffeinated coffee."],
     "ru": ["{S} пьёт только кофе без кофеина.", "{S} признаёт только декаф."]},
])
slot("alcohol", "food", [
    {"en": ["{S} doesn't drink alcohol at all.", "{S} is teetotal."],
     "ru": ["{S} совсем не пьёт алкоголь.", "{S} не употребляет спиртное."]},
    {"en": ["{S} enjoys a glass of red wine with dinner.", "{S} likes red wine and has some most evenings."],
     "ru": ["{S} любит красное вино и выпивает бокал за ужином.", "{S} почти каждый вечер выпивает бокал красного вина."]},
    {"en": ["{S} prefers craft beer.", "{S} drinks beer, mostly local craft brews."],
     "ru": ["{S} предпочитает крафтовое пиво.", "{S} пьёт пиво, в основном местный крафт."]},
])
slot("fav_dish", "food", [
    {"en": ["{P} favourite dish is borscht.", "{S} loves borscht more than any other dish."],
     "ru": ["Любимое блюдо {G} — борщ.", "{S} больше всего на свете любит борщ."]},
    {"en": ["{P} favourite dish is pad thai.", "{S} would eat pad thai every day if [he|she] could."],
     "ru": ["Любимое блюдо {G} — пад тай.", "{S} готов[|а] есть пад тай каждый день."]},
    {"en": ["{P} favourite dish is lasagna.", "{S} says lasagna is [his|her] favourite food."],
     "ru": ["Любимое блюдо {G} — лазанья.", "{S} говорит, что лазанья — [его|её] любимая еда."]},
    {"en": ["{P} favourite dish is khachapuri.", "{S} adores Adjarian khachapuri above all."],
     "ru": ["Любимое блюдо {G} — хачапури.", "{S} больше всего любит хачапури по-аджарски."]},
])
slot("smoking", "health", [
    {"en": ["{S} is a non-smoker.", "{S} doesn't smoke."],
     "ru": ["{S} не курит.", "{S} — [некурящий|некурящая]."]},
    {"en": ["{S} smokes about half a pack a day.", "{S} is a smoker."],
     "ru": ["{S} курит, примерно полпачки в день.", "{S} — [курильщик|курильщица]."]},
    {"en": ["{S} uses a vape instead of cigarettes.", "{S} vapes."],
     "ru": ["{S} курит электронные сигареты.", "{S} пользуется вейпом вместо сигарет."]},
])
slot("sleep", "health", [
    {"en": ["{S} is an early riser and gets up at 5:30.", "{S} wakes up at half past five every morning."],
     "ru": ["{S} — жаворонок, встаёт в 5:30.", "{S} просыпается каждое утро в половине шестого."]},
    {"en": ["{S} is a night owl and rarely sleeps before 2 a.m.", "{S} usually goes to bed after two at night."],
     "ru": ["{S} — сова, редко ложится раньше двух ночи.", "{S} обычно засыпает после двух часов ночи."]},
])
slot("glasses", "health", [
    {"en": ["{S} wears glasses.", "{S} needs glasses to see properly."],
     "ru": ["{S} носит очки.", "{S} без очков плохо видит."]},
    {"en": ["{S} wears contact lenses.", "{S} uses daily contact lenses."],
     "ru": ["{S} носит контактные линзы.", "{S} пользуется однодневными линзами."]},
    {"en": ["{S} has perfect eyesight and needs no glasses.", "{S} sees perfectly without glasses."],
     "ru": ["У {G} отличное зрение, очки не нужны.", "{S} прекрасно видит без очков."]},
])
slot("blood_type", "health", [
    {"en": ["{P} blood type is A positive.", "{S} has blood group A+."],
     "ru": ["Группа крови у {G} — вторая положительная.", "У {G} группа крови A(II), резус плюс."]},
    {"en": ["{P} blood type is O negative.", "{S} has blood group O-."],
     "ru": ["Группа крови у {G} — первая отрицательная.", "У {G} группа крови 0(I), резус минус."]},
    {"en": ["{P} blood type is B positive.", "{S} has blood group B+."],
     "ru": ["Группа крови у {G} — третья положительная.", "У {G} группа крови B(III), резус плюс."]},
], change=False)
slot("pet", "pets", [
    {"en": ["{S} has a beagle called Biscuit.", "{P} dog Biscuit is a beagle."],
     "ru": ["У {G} есть бигль по кличке Бисквит.", "Пёс {G} Бисквит — бигль."]},
    {"en": ["{S} has a poodle called Biscuit.", "{P} dog Biscuit is a poodle."],
     "ru": ["У {G} есть пудель по кличке Бисквит.", "Пёс {G} Бисквит — пудель."]},
    {"en": ["{S} has a dachshund called Biscuit.", "{P} dog Biscuit is a dachshund."],
     "ru": ["У {G} есть такса по кличке Бисквит.", "Пёс {G} Бисквит — такса."]},
    {"en": ["{S} has no pets.", "{S} doesn't keep any animals at home."],
     "ru": ["У {G} нет домашних животных.", "{S} не держит дома никаких животных."]},
])
slot("band_role", "music", [
    {"en": ["{S} plays bass in an amateur rock band.", "In [his|her] amateur band {s} is the bassist."],
     "ru": ["{S} играет на басу в любительской рок-группе.", "В любительской группе {s} — басист."]},
    {"en": ["{S} plays drums in an amateur rock band.", "In [his|her] amateur band {s} is the drummer."],
     "ru": ["{S} играет на барабанах в любительской рок-группе.", "В любительской группе {s} — барабанщик."]},
    {"en": ["{S} is the lead singer of an amateur rock band.", "In [his|her] amateur band {s} sings lead vocals."],
     "ru": ["{S} — [солист|солистка] любительской рок-группы.", "В любительской группе {s} поёт основной вокал."]},
])
slot("music", "music", [
    {"en": ["{S} mostly listens to jazz.", "Jazz is {p} favourite music."],
     "ru": ["{S} слушает в основном джаз.", "Любимая музыка {G} — джаз."]},
    {"en": ["{S} mostly listens to heavy metal.", "Heavy metal is {p} favourite music."],
     "ru": ["{S} слушает в основном хеви-метал.", "Любимая музыка {G} — хеви-метал."]},
    {"en": ["{S} mostly listens to classical music.", "Classical music is what {s} loves most."],
     "ru": ["{S} слушает в основном классику.", "Больше всего {s} любит классическую музыку."]},
    {"en": ["{S} mostly listens to techno.", "Techno is {p} favourite music."],
     "ru": ["{S} слушает в основном техно.", "Любимая музыка {G} — техно."]},
])
slot("football_position", "sport", [
    {"en": ["{S} plays goalkeeper in an amateur football team.", "In amateur football {s} is the keeper."],
     "ru": ["{S} стоит на воротах в любительской футбольной команде.", "В любительском футболе {s} — вратарь."]},
    {"en": ["{S} plays as a defender in an amateur football team.", "In amateur football {s} plays at the back."],
     "ru": ["{S} играет в защите в любительской футбольной команде.", "В любительском футболе {s} — защитник."]},
    {"en": ["{S} plays as a striker in an amateur football team.", "In amateur football {s} plays up front."],
     "ru": ["{S} играет в нападении в любительской футбольной команде.", "В любительском футболе {s} — нападающий."]},
])
slot("team", "sport", [
    {"en": ["{S} supports Benfica.", "{S} is a lifelong Benfica fan."],
     "ru": ["{S} болеет за «Бенфику».", "{S} с детства фанат «Бенфики»."]},
    {"en": ["{S} supports Red Star Belgrade.", "{S} is a Red Star Belgrade fan."],
     "ru": ["{S} болеет за «Црвену Звезду».", "{S} — фанат белградской «Црвены Звезды»."]},
    {"en": ["{S} supports Rubin Kazan.", "{S} is a fan of Rubin Kazan."],
     "ru": ["{S} болеет за казанский «Рубин».", "{S} — фанат «Рубина»."]},
], change=False)
slot("spouse", "family", [
    {"en": ["{S} is married to Sasha.", "{P} spouse is Sasha."],
     "ru": ["{S} [женат на Саше|замужем за Сашей].", "Супруг[а|] {G} — Саша."]},
    {"en": ["{S} is single.", "{S} isn't married and isn't seeing anyone."],
     "ru": ["{S} [холост|не замужем].", "{S} не в браке и ни с кем не встречается."]},
    {"en": ["{S} is divorced.", "{S} got divorced some years ago and lives alone."],
     "ru": ["{S} в разводе.", "{S} [развёлся|развелась] несколько лет назад и живёт [один|одна]."]},
    {"en": ["{S} lives with partner Nika but they are not married.", "{S} and Nika are a couple, unmarried."],
     "ru": ["{S} живёт с Никой, но они не расписаны.", "{S} и Ника — пара, но не в браке."]},
])
slot("children", "family", [
    {"en": ["{S} has no children.", "{S} doesn't have kids."],
     "ru": ["У {G} нет детей.", "{S} бездетн[ый|ая]."]},
    {"en": ["{S} has one son, Tim.", "{P} only child is a son named Tim."],
     "ru": ["У {G} один сын, Тима.", "Единственный ребёнок {G} — сын Тима."]},
    {"en": ["{S} has two daughters.", "{S} is the parent of two girls."],
     "ru": ["У {G} две дочери.", "{S} растит двух дочек."]},
    {"en": ["{S} has three children.", "{S} is a parent of three kids."],
     "ru": ["У {G} трое детей.", "{S} — [отец|мать] троих детей."]},
])
slot("sibling", "family", [
    {"en": ["{S} has an older brother, Ilya.", "{P} big brother is called Ilya."],
     "ru": ["У {G} есть старший брат Илья.", "Старшего брата {G} зовут Илья."]},
    {"en": ["{S} is an only child.", "{S} has no brothers or sisters."],
     "ru": ["{S} — единственный ребёнок в семье.", "У {G} нет ни братьев, ни сестёр."]},
    {"en": ["{S} has a twin sister, Vera.", "{P} twin sister is called Vera."],
     "ru": ["У {G} есть сестра-близнец Вера.", "Сестру-близнеца {G} зовут Вера."]},
], change=False)
slot("birthday", "dates", [
    {"en": ["{S} was born on 3 March.", "{P} birthday is on the 3rd of March."],
     "ru": ["{S} [родился|родилась] 3 марта.", "День рождения у {G} 3 марта."]},
    {"en": ["{S} was born on 17 July.", "{P} birthday is on 17 July."],
     "ru": ["{S} [родился|родилась] 17 июля.", "День рождения у {G} 17 июля."]},
    {"en": ["{S} was born on 29 October.", "{P} birthday falls on 29 October."],
     "ru": ["{S} [родился|родилась] 29 октября.", "День рождения у {G} 29 октября."]},
    {"en": ["{S} was born on 11 December.", "{P} birthday is on the 11th of December."],
     "ru": ["{S} [родился|родилась] 11 декабря.", "День рождения у {G} 11 декабря."]},
], change=False)
slot("degree", "education", [
    {"en": ["{S} studied chemistry at university.", "{S} has a degree in chemistry."],
     "ru": ["{S} [учился|училась] в университете на химика.", "У {G} диплом химика."]},
    {"en": ["{S} studied law.", "{S} has a law degree."],
     "ru": ["{S} [окончил|окончила] юрфак.", "У {G} юридическое образование."]},
    {"en": ["{S} studied architecture.", "{S} graduated in architecture."],
     "ru": ["{S} [учился|училась] на архитектора.", "У {G} архитектурное образование."]},
    {"en": ["{S} studied history.", "{S} has a degree in history."],
     "ru": ["{S} [окончил|окончила] истфак.", "У {G} диплом историка."]},
], change=False)
slot("native_language", "language", [
    {"en": ["{P} native language is Russian.", "{S} grew up speaking Russian."],
     "ru": ["Родной язык {G} — русский.", "{S} [вырос|выросла], говоря по-русски."]},
    {"en": ["{P} native language is Serbian.", "{S} grew up speaking Serbian."],
     "ru": ["Родной язык {G} — сербский.", "{S} [вырос|выросла], говоря по-сербски."]},
    {"en": ["{P} native language is Georgian.", "{S} grew up speaking Georgian."],
     "ru": ["Родной язык {G} — грузинский.", "{S} [вырос|выросла], говоря по-грузински."]},
], change=False)
slot("learning_language", "language", [
    {"en": ["{S} is learning Japanese.", "{S} takes Japanese lessons twice a week."],
     "ru": ["{S} учит японский.", "{S} дважды в неделю ходит на японский."]},
    {"en": ["{S} is learning Spanish.", "{S} takes Spanish lessons twice a week."],
     "ru": ["{S} учит испанский.", "{S} дважды в неделю занимается испанским."]},
    {"en": ["{S} is learning German.", "{S} studies German with a tutor."],
     "ru": ["{S} учит немецкий.", "{S} занимается немецким с репетитором."]},
    {"en": ["{S} is learning Turkish.", "{S} studies Turkish in evening classes."],
     "ru": ["{S} учит турецкий.", "{S} ходит на вечерние курсы турецкого."]},
])
slot("holiday", "travel", [
    {"en": ["{S} prefers holidays in the mountains.", "For a holiday {s} always picks the mountains."],
     "ru": ["{S} предпочитает отдыхать в горах.", "В отпуск {s} всегда едет в горы."]},
    {"en": ["{S} prefers beach holidays.", "For a holiday {s} always heads to the sea."],
     "ru": ["{S} предпочитает пляжный отдых.", "В отпуск {s} всегда едет на море."]},
    {"en": ["{S} prefers city breaks with museums.", "On holiday {s} likes cities and museums, not nature."],
     "ru": ["{S} любит отдыхать в городах, с музеями.", "В отпуске {s} выбирает города и музеи, а не природу."]},
])
slot("film", "leisure", [
    {"en": ["{S} loves horror films.", "Horror is {p} favourite film genre."],
     "ru": ["{S} обожает фильмы ужасов.", "Любимый жанр кино у {G} — ужастики."]},
    {"en": ["{S} mostly watches documentaries.", "Documentaries are {p} favourite kind of film."],
     "ru": ["{S} смотрит в основном документальное кино.", "Любимый жанр кино у {G} — документалки."]},
    {"en": ["{S} loves romantic comedies.", "Romcoms are {p} favourite films."],
     "ru": ["{S} обожает романтические комедии.", "Любимый жанр кино у {G} — ромкомы."]},
    {"en": ["{S} loves science fiction films.", "Sci-fi is {p} favourite film genre."],
     "ru": ["{S} обожает научную фантастику в кино.", "Любимый жанр кино у {G} — фантастика."]},
])
slot("fav_colour", "leisure", [
    {"en": ["{P} favourite colour is green.", "{S} likes green best of all colours."],
     "ru": ["Любимый цвет {G} — зелёный.", "Из всех цветов {s} больше всего любит зелёный."]},
    {"en": ["{P} favourite colour is yellow.", "{S} likes yellow best of all colours."],
     "ru": ["Любимый цвет {G} — жёлтый.", "Из всех цветов {s} больше всего любит жёлтый."]},
    {"en": ["{P} favourite colour is navy blue.", "{S} likes navy blue best."],
     "ru": ["Любимый цвет {G} — тёмно-синий.", "Больше всего {s} любит тёмно-синий цвет."]},
])
slot("shoe_size", "clothes", [
    {"en": ["{P} shoe size is 38 EU.", "{S} wears size 38 shoes."],
     "ru": ["Размер обуви у {G} — 38.", "{S} носит обувь 38-го размера."]},
    {"en": ["{P} shoe size is 41 EU.", "{S} wears size 41 shoes."],
     "ru": ["Размер обуви у {G} — 41.", "{S} носит обувь 41-го размера."]},
    {"en": ["{P} shoe size is 45 EU.", "{S} wears size 45 shoes."],
     "ru": ["Размер обуви у {G} — 45.", "{S} носит обувь 45-го размера."]},
], change=False)
slot("messenger", "contact", [
    {"en": ["{S} prefers to be reached on Telegram.", "Telegram is the best way to reach {o}."],
     "ru": ["Для связи {S} выбирает Telegram.", "{S} предпочитает, чтобы писали в Telegram."]},
    {"en": ["{S} prefers to be reached by email.", "Email is the best way to reach {o}."],
     "ru": ["Для связи {S} выбирает электронную почту.", "{S} предпочитает, чтобы писали на e-mail."]},
    {"en": ["{S} prefers phone calls to messages.", "{S} would rather get a call than a text."],
     "ru": ["{S} предпочитает звонки сообщениям.", "{S} больше любит, когда звонят, а не пишут."]},
    {"en": ["{S} prefers to be reached on Signal.", "Signal is the best way to reach {o}."],
     "ru": ["Для связи {S} выбирает Signal.", "{S} предпочитает, чтобы писали в Signal."]},
])
slot("weekends", "leisure", [
    {"en": ["{S} usually spends weekends at the dacha.", "Most weekends {s} is at the country house."],
     "ru": ["{S} обычно проводит выходные на даче.", "Почти все выходные {s} на даче."]},
    {"en": ["{S} usually spends weekends hiking.", "Most weekends {s} goes hiking."],
     "ru": ["{S} обычно проводит выходные в походах.", "Почти каждые выходные {s} уходит в поход."]},
    {"en": ["{S} usually spends weekends at home playing video games.", "Most weekends {s} stays in and plays video games."],
     "ru": ["{S} обычно проводит выходные дома за видеоиграми.", "Почти все выходные {s} сидит дома и играет в видеоигры."]},
])

# Hard pairs that share topic words across groups (used for "hard none" picking too).
NEAR_GROUPS = {
    "home": ["family", "travel", "transport"], "family": ["home"], "transport": ["home", "work"],
    "work": ["transport", "education"], "food": ["health"], "health": ["food", "sport"],
    "music": ["leisure", "sport"], "sport": ["music", "health"], "language": ["education", "travel"],
    "education": ["work", "language"], "travel": ["home", "leisure"], "leisure": ["music", "travel"],
    "pets": ["home"], "dates": ["family"], "clothes": ["leisure"], "contact": ["work"],
}

# Russian change markers for supersede; English ones. {mon} etc. filled per case.
CHANGE_EN_PRE = ["Since {mon}, ", "As of {mon} {yr}, ", "Recently ", "Since last {season}, ", "These days ", "Update: "]
CHANGE_EN_POST = [" (that changed in {mon})", " now, not as before", " since {mon} {yr}"]
CHANGE_RU_PRE = ["С {mong} ", "С {yr} года ", "Недавно поменялось: ", "Теперь ", "С прошлой {season_ru} "]
CHANGE_RU_POST = [" (так с {mong})", ", раньше было иначе", " — с {mong} {yr} года"]
NEUTRAL_EN_PRE = ["", "", "", "Note: ", "FYI: "]
NEUTRAL_RU_PRE = ["", "", "", "На заметку: ", "Кстати, "]
SEASONS_EN = ["spring", "summer", "autumn", "winter"]
SEASONS_RU = ["весны", "осени", "зимы"]  # «с прошлой весны/осени/зимы»

# ---- episodic facts (events; compatible with each other) ----------------------------------

ITEMS = [("a new laptop", "новый ноутбук"), ("a used upright piano", "подержанное пианино"),
         ("an e-bike", "электровелосипед"), ("a grey corner sofa", "серый угловой диван"),
         ("a small drone", "маленький дрон"), ("a film camera", "плёночный фотоаппарат"),
         ("a coffee machine", "кофемашину"), ("hiking boots", "треккинговые ботинки"),
         ("an aquarium", "аквариум"), ("a sewing machine", "швейную машинку")]
FRIENDS = [("Lena", "Леной"), ("Bogdan", "Богданом"), ("Tamar", "Тамар"), ("Kostya", "Костей"),
           ("Mira", "Мирой"), ("Gosha", "Гошей"), ("Zoya", "Зоей"), ("Rustam", "Рустамом")]
PLACES = [("Café Lumen", "кафе «Люмен»"), ("the Georgian place on the corner", "грузинском ресторанчике на углу"),
          ("a ramen bar", "раменной"), ("the botanical garden", "ботаническом саду"),
          ("the old market", "старом рынке"), ("a rooftop bar", "баре на крыше")]
BANDS = [("Nautilus", "«Наутилуса»"), ("Radiohead tribute band", "трибьют-группы Radiohead"),
         ("a local jazz trio", "местного джаз-трио"), ("the city symphony orchestra", "городского симфонического оркестра"),
         ("Kino tribute band", "трибьют-группы «Кино»")]
COURSES = [("Python", "Python"), ("watercolour painting", "акварели"), ("first aid", "первой помощи"),
           ("bread baking", "выпечке хлеба"), ("public speaking", "ораторскому мастерству"),
           ("photography", "фотографии"), ("personal finance", "личным финансам")]
BOOKS = [("The Master and Margarita", "«Мастер и Маргарита»"), ("Dune", "«Дюна»"),
         ("War and Peace", "«Война и мир»"), ("Solaris", "«Солярис»"), ("The Hobbit", "«Хоббит»"),
         ("Roadside Picnic", "«Пикник на обочине»"), ("Norwegian Wood", "«Норвежский лес»")]
DOCTORS = [("the dentist", "стоматолога"), ("an eye doctor", "окулиста"), ("a physiotherapist", "физиотерапевта"),
           ("a dermatologist", "дерматолога")]

EVENTS = {
    "trip": {
        "args": lambda r: (r.randrange(len(CITIES)), r.randrange(12), r.choice([2023, 2024, 2025])),
        "en": ["{S} went to {c} in {mon} {yr}.", "{S} spent a week in {c} in {mon} {yr}.",
               "In {mon} {yr} {s} travelled to {c}."],
        "ru": ["{S} [ездил|ездила] в {ca} в {monp} {yr} года.", "В {monp} {yr} года {s} [провёл|провела] неделю в {cp}.",
               "{S} [съездил|съездила] в {ca} — это было в {monp} {yr} года."],
    },
    "purchase": {
        "args": lambda r: (r.randrange(len(ITEMS)), r.randrange(12), r.choice([2024, 2025])),
        "en": ["{S} bought {item} in {mon} {yr}.", "In {mon} {yr} {s} got {item}.", "{S} treated [himself|herself] to {item} in {mon} {yr}."],
        "ru": ["{S} [купил|купила] {item_ru} в {monp} {yr} года.", "В {monp} {yr} года {s} [приобрёл|приобрела] {item_ru}.",
               "{S} [порадовал|порадовала] себя покупкой: {item_ru}, {monp} {yr} года."],
    },
    "meet": {
        "args": lambda r: (r.randrange(len(FRIENDS)), r.randrange(len(PLACES)), r.randrange(12)),
        "en": ["{S} had dinner with {fr} at {pl} in {mon}.", "In {mon} {s} met {fr} at {pl}.", "{S} and {fr} caught up over food at {pl} in {mon}."],
        "ru": ["{S} [ужинал|ужинала] с {fr_ru} в {pl_ru} в {monp}.", "В {monp} {s} [встретился|встретилась] с {fr_ru} в {pl_ru}.",
               "{S} [посидел|посидела] с {fr_ru} в {pl_ru} — это было в {monp}."],
    },
    "concert": {
        "args": lambda r: (r.randrange(len(BANDS)), r.randrange(12), r.choice([2024, 2025])),
        "en": ["{S} went to a concert of {band} in {mon} {yr}.", "In {mon} {yr} {s} saw {band} live."],
        "ru": ["{S} [ходил|ходила] на концерт {band_ru} в {monp} {yr} года.", "В {monp} {yr} года {s} [слушал|слушала] вживую {band_ru}."],
    },
    "course": {
        "args": lambda r: (r.randrange(len(COURSES)), r.randrange(12), r.choice([2023, 2024, 2025])),
        "en": ["{S} finished an online course in {course} in {mon} {yr}.", "In {mon} {yr} {s} completed a {course} course."],
        "ru": ["{S} [закончил|закончила] онлайн-курс по {course_ru} в {monp} {yr} года.", "В {monp} {yr} года {s} [прошёл|прошла] курс по {course_ru}."],
    },
    "book": {
        "args": lambda r: (r.randrange(len(BOOKS)), r.randrange(12), r.choice([2024, 2025])),
        "en": ["{S} read {book} in {mon} {yr} and liked it.", "In {mon} {yr} {s} finished reading {book}."],
        "ru": ["{S} [прочитал|прочитала] {book_ru} в {monp} {yr} года, понравилось.", "В {monp} {yr} года {s} [дочитал|дочитала] {book_ru}."],
    },
    "doctor": {
        "args": lambda r: (r.randrange(len(DOCTORS)), r.randrange(12), r.choice([2024, 2025])),
        "en": ["{S} saw {doc} in {mon} {yr}.", "In {mon} {yr} {s} had an appointment with {doc}."],
        "ru": ["{S} [был|была] у {doc_ru} в {monp} {yr} года.", "В {monp} {yr} года {s} [ходил|ходила] к {doc_ru_dat}."],
    },
}
DOCTOR_DAT = {"стоматолога": "стоматологу", "окулиста": "окулисту", "физиотерапевта": "физиотерапевту",
              "дерматолога": "дерматологу"}
